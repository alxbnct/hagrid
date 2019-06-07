use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::{create_dir_all, read_link, remove_file, rename, set_permissions, Permissions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::os::unix::fs::PermissionsExt;

use tempfile;
use url;
use pathdiff::diff_paths;

//use sequoia_openpgp::armor::{Writer, Kind};

use {Database, Query};
use types::{Email, Fingerprint, KeyID};
use sync::FlockMutexGuard;
use Result;

use tempfile::NamedTempFile;

use openpgp::TPK;

pub struct Filesystem {
    tmp_dir: PathBuf,

    keys_internal_dir: PathBuf,
    keys_external_dir: PathBuf,
    keys_dir_full: PathBuf,
    keys_dir_quarantined: PathBuf,
    keys_dir_published: PathBuf,

    links_dir_by_fingerprint: PathBuf,
    links_dir_by_keyid: PathBuf,
    links_dir_by_email: PathBuf,

    dry_run: bool,
}

/// Returns the given path, ensuring that the parent directory exists.
///
/// Use this on paths returned by .path_to_* before creating the
/// object.
fn ensure_parent(path: &Path) -> Result<&Path> {
    let parent = path.parent().unwrap();
    create_dir_all(parent)?;
    Ok(path)
}


impl Filesystem {
    pub fn new_from_base(base_dir: impl Into<PathBuf>) -> Result<Self> {
        let base_dir: PathBuf = base_dir.into();

        let keys_dir = base_dir.join("keys");
        let tmp_dir = base_dir.join("tmp");

        Self::new(&keys_dir, &keys_dir, tmp_dir)
    }

    pub fn new(
        keys_internal_dir: impl Into<PathBuf>,
        keys_external_dir: impl Into<PathBuf>,
        tmp_dir: impl Into<PathBuf>,
    ) -> Result<Self> {
        Self::new_internal(keys_internal_dir, keys_external_dir, tmp_dir, false)
    }

    pub fn new_internal(
        keys_internal_dir: impl Into<PathBuf>,
        keys_external_dir: impl Into<PathBuf>,
        tmp_dir: impl Into<PathBuf>,
        dry_run: bool,
    ) -> Result<Self> {
        let tmp_dir = tmp_dir.into();
        create_dir_all(&tmp_dir)?;

        let keys_internal_dir: PathBuf = keys_internal_dir.into();
        let keys_external_dir: PathBuf = keys_external_dir.into();
        let keys_dir_full = keys_internal_dir.join("full");
        let keys_dir_quarantined = keys_internal_dir.join("quarantined");
        let keys_dir_published = keys_external_dir.join("pub");
        create_dir_all(&keys_dir_full)?;
        create_dir_all(&keys_dir_quarantined)?;
        create_dir_all(&keys_dir_published)?;

        let links_dir = keys_external_dir.join("links");
        let links_dir_by_keyid = links_dir.join("by-keyid");
        let links_dir_by_fingerprint = links_dir.join("by-fpr");
        let links_dir_by_email = links_dir.join("by-email");
        create_dir_all(&links_dir_by_keyid)?;
        create_dir_all(&links_dir_by_fingerprint)?;
        create_dir_all(&links_dir_by_email)?;

        info!("Opened filesystem database.");
        info!("keys_internal_dir: '{}'", keys_internal_dir.display());
        info!("keys_external_dir: '{}'", keys_external_dir.display());
        info!("tmp_dir: '{}'", tmp_dir.display());
        Ok(Filesystem {
            keys_internal_dir,
            keys_external_dir,
            tmp_dir,

            keys_dir_full,
            keys_dir_published,
            keys_dir_quarantined,

            links_dir_by_keyid,
            links_dir_by_fingerprint,
            links_dir_by_email,

            dry_run,
        })
    }

    /// Returns the path to the given Fingerprint.
    fn fingerprint_to_path_full(&self, fingerprint: &Fingerprint) -> PathBuf {
        let hex = fingerprint.to_string();
        self.keys_dir_full.join(path_split(&hex))
    }

    /// Returns the path to the given Fingerprint.
    fn fingerprint_to_path_quarantined(&self, fingerprint: &Fingerprint) -> PathBuf {
        let hex = fingerprint.to_string();
        self.keys_dir_quarantined.join(&hex)
    }

    /// Returns the path to the given Fingerprint.
    fn fingerprint_to_path_published(&self, fingerprint: &Fingerprint) -> PathBuf {
        let hex = fingerprint.to_string();
        self.keys_dir_published.join(path_split(&hex))
    }

    /// Returns the path to the given KeyID.
    fn link_by_keyid(&self, keyid: &KeyID) -> PathBuf {
        let hex = keyid.to_string();
        self.links_dir_by_keyid.join(path_split(&hex))
    }

    /// Returns the path to the given Fingerprint.
    fn link_by_fingerprint(&self, fingerprint: &Fingerprint) -> PathBuf {
        let hex = fingerprint.to_string();
        self.links_dir_by_fingerprint.join(path_split(&hex))
    }

    /// Returns the path to the given Email.
    fn link_by_email(&self, email: &Email) -> PathBuf {
        let email =
            url::form_urlencoded::byte_serialize(email.as_str().as_bytes())
                .collect::<String>();
        self.links_dir_by_email.join(path_split(&email))
    }

    fn read_from_path(&self, path: &Path, allow_internal: bool) -> Option<String> {
        use std::fs;

        if !path.starts_with(&self.keys_external_dir) &&
            !(allow_internal && path.starts_with(&self.keys_internal_dir)) {
            panic!("Attempted to access file outside expected dirs!");
        }

        if path.exists() {
            fs::read_to_string(path).ok()
        } else {
            None
        }
    }

    /// Returns the Fingerprint the given path is pointing to.
    pub fn path_to_fingerprint(path: &Path) -> Option<Fingerprint> {
        use std::str::FromStr;
        let merged = path_merge(path);
        Fingerprint::from_str(&merged).ok()
    }

    /// Returns the KeyID the given path is pointing to.
    fn path_to_keyid(path: &Path) -> Option<KeyID> {
        use std::str::FromStr;
        let merged = path_merge(path);
        KeyID::from_str(&merged).ok()
    }

    /// Returns the Email the given path is pointing to.
    fn path_to_email(path: &Path) -> Option<Email> {
        use std::str::FromStr;
        let merged = path_merge(path);
        let decoded = url::form_urlencoded::parse(merged.as_bytes()).next()?.0;
        Email::from_str(&decoded).ok()
    }

    /// Returns the backing primary key fingerprint for any key path.
    fn path_to_primary(path: &Path) -> Option<Fingerprint> {
        use std::fs;
        let typ = fs::symlink_metadata(&path).ok()?.file_type();
        if typ.is_symlink() {
            let path = read_link(path).ok()?;
            Filesystem::path_to_fingerprint(&path)
        } else {
            Filesystem::path_to_fingerprint(path)
        }
    }

    fn perform_checks(
        &self,
        checks_dir: &Path,
        tpks: &mut HashMap<Fingerprint, TPK>,
        check: impl Fn(&Path, &TPK, &Fingerprint) -> Result<()>,
    ) -> Result<()> {
        use walkdir::WalkDir;
        use std::fs;
        use failure::format_err;

        for entry in WalkDir::new(checks_dir) {
            let entry = entry?;
            let path = entry.path();
            let typ = fs::symlink_metadata(&path)?.file_type();
            if typ.is_dir() {
                continue;
            }

            // Compute the corresponding primary fingerprint just
            // by looking at the paths.
            let primary_fp = Filesystem::path_to_primary(path)
                .ok_or_else(
                            || format_err!("Malformed path: {:?}",
                                            path.read_link().unwrap()))?;
            // Load into cache.
            if ! tpks.contains_key(&primary_fp) {
                tpks.insert(
                    primary_fp.clone(),
                    self.lookup(&Query::ByFingerprint(primary_fp.clone()))
                        ?.ok_or_else(
                            || format_err!("No TPK with fingerprint {:?}",
                                            primary_fp))?);
            }

            let tpk = tpks.get(&primary_fp)
                .ok_or_else(
                    || format_err!("Broken symlink {:?}: No such Key {}",
                                    path, primary_fp))?;

            check(&path, &tpk, &primary_fp)?;
        }

        Ok(())
    }
}

// Like `symlink`, but instead of failing if `symlink_name` already
// exists, atomically update `symlink_name` to have `symlink_content`.
fn symlink(symlink_content: &Path, symlink_name: &Path) -> Result<()> {
    use std::os::unix::fs::{symlink};

    let symlink_dir = ensure_parent(symlink_name)?.parent().unwrap();
    let tmp_dir = tempfile::Builder::new()
        .prefix("link")
        .rand_bytes(16)
        .tempdir_in(symlink_dir)?;
    let symlink_name_tmp = tmp_dir.path().join("link");

    symlink(&symlink_content, &symlink_name_tmp)?;
    rename(&symlink_name_tmp, &symlink_name)?;
    Ok(())
}

impl Database for Filesystem {
    type MutexGuard = FlockMutexGuard;

    fn lock(&self) -> Result<Self::MutexGuard> {
        FlockMutexGuard::lock(&self.keys_internal_dir)
    }

    fn write_to_temp(&self, content: &[u8]) -> Result<NamedTempFile> {
        let mut tempfile = tempfile::Builder::new()
            .prefix("key")
            .rand_bytes(16)
            .tempfile_in(&self.tmp_dir)?;
        tempfile.write_all(content).unwrap();
        Ok(tempfile)
    }

    fn move_tmp_to_full(&self, file: NamedTempFile, fpr: &Fingerprint) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        set_permissions(file.path(), Permissions::from_mode(0o640))?;
        let target = self.fingerprint_to_path_full(fpr);
        file.persist(ensure_parent(&target)?)?;
        Ok(())
    }

    fn move_tmp_to_published(&self, file: NamedTempFile, fpr: &Fingerprint) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        set_permissions(file.path(), Permissions::from_mode(0o644))?;
        let target = self.fingerprint_to_path_published(fpr);
        file.persist(ensure_parent(&target)?)?;
        Ok(())
    }

    fn write_to_quarantine(&self, fpr: &Fingerprint, content: &[u8]) -> Result<()> {
        let mut tempfile = tempfile::Builder::new()
            .prefix("key")
            .rand_bytes(16)
            .tempfile_in(&self.tmp_dir)?;
        tempfile.write_all(content).unwrap();

        let target = self.fingerprint_to_path_quarantined(fpr);
        tempfile.persist(ensure_parent(&target)?)?;

        Ok(())
    }

    fn check_link_fpr(&self, fpr: &Fingerprint, fpr_target: &Fingerprint) -> Result<Option<Fingerprint>> {
        let link_keyid = self.link_by_keyid(&fpr.into());
        let link_fpr = self.link_by_fingerprint(&fpr);

        let path_published = self.fingerprint_to_path_published(fpr_target);

        if let Ok(link_fpr_target) = link_keyid.canonicalize() {
             if !link_fpr_target.ends_with(&path_published) {
                info!("Fingerprint points to different key for {} (expected {:?} to be suffix of {:?})",
                    fpr, &path_published, &link_fpr_target);
                Err(failure::err_msg(format!("Fingerprint collision for key {}", fpr)))?;
             }
        }

        if let Ok(link_keyid_target) = link_keyid.canonicalize() {
            if !link_keyid_target.ends_with(&path_published) {
                info!("KeyID points to different key for {} (expected {:?} to be suffix of {:?})",
                    fpr, &path_published, &link_keyid_target);
                Err(failure::err_msg(format!("KeyID collision for key {}", fpr)))?;
            }
        }

        if !link_fpr.exists() || !link_keyid.exists() {
            Ok(Some(fpr.clone()))
        } else {
            Ok(None)
        }
    }

    fn lookup_primary_fingerprint(&self, term: &Query) -> Option<Fingerprint> {
        use super::Query::*;
        let path = match term {
            ByFingerprint(ref fp) => self.link_by_fingerprint(fp),
            ByKeyID(ref keyid) => self.link_by_keyid(keyid),
            ByEmail(ref email) => self.link_by_email(email),
        };
        path.read_link()
            .ok()
            .and_then(|link_path| Filesystem::path_to_fingerprint(&link_path))
    }

    /// Gets the path to the underlying file, if any.
    fn lookup_path(&self, term: &Query) -> Option<PathBuf> {
        use super::Query::*;
        let path = match term {
            ByFingerprint(ref fp) => self.link_by_fingerprint(fp),
            ByKeyID(ref keyid) => self.link_by_keyid(keyid),
            ByEmail(ref email) => self.link_by_email(email),
        };

        if path.exists() {
            let x = diff_paths(&path, &self.keys_external_dir).expect("related paths");
            Some(x)
        } else {
            None
        }
    }

    fn link_email(&self, email: &Email, fpr: &Fingerprint) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        let link = self.link_by_email(&email);
        let target = diff_paths(&self.fingerprint_to_path_published(fpr),
                                link.parent().unwrap()).unwrap();

        if link == target {
            return Ok(());
        }

        symlink(&target, ensure_parent(&link)?)
    }

    fn unlink_email(&self, email: &Email, fpr: &Fingerprint) -> Result<()> {
        let link = self.link_by_email(&email);

        match read_link(&link) {
            Ok(target) => {
                let expected = diff_paths(&self.fingerprint_to_path_published(fpr),
                                          link.parent().unwrap()).unwrap();

                if target == expected {
                    remove_file(link)?;
                }
            }
            Err(_) => {}
        }

        Ok(())
    }

    fn link_fpr(&self, from: &Fingerprint, primary_fpr: &Fingerprint) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        let link_fpr = self.link_by_fingerprint(from);
        let link_keyid = self.link_by_keyid(&from.into());
        let target = diff_paths(&self.fingerprint_to_path_published(primary_fpr),
                                link_fpr.parent().unwrap()).unwrap();

        symlink(&target, ensure_parent(&link_fpr)?)?;
        symlink(&target, ensure_parent(&link_keyid)?)
    }

    fn unlink_fpr(&self, from: &Fingerprint, primary_fpr: &Fingerprint) -> Result<()> {
        let link_fpr = self.link_by_fingerprint(from);
        let link_keyid = self.link_by_keyid(&from.into());
        let expected = diff_paths(&self.fingerprint_to_path_published(primary_fpr),
                                link_fpr.parent().unwrap()).unwrap();

        match read_link(&link_fpr) {
            Ok(target) => {
                if target == expected {
                    remove_file(&link_fpr)?;
                }
            }
            Err(_) => {}
        }
        match read_link(&link_keyid) {
            Ok(target) => {
                if target == expected {
                    remove_file(link_keyid)?;
                }
            }
            Err(_) => {}
        }

        Ok(())
    }

    // XXX: slow
    fn by_fpr_full(&self, fpr: &Fingerprint) -> Option<String> {
        let path = self.fingerprint_to_path_full(fpr);
        self.read_from_path(&path, true)
    }

    // XXX: slow
    fn by_primary_fpr(&self, fpr: &Fingerprint) -> Option<String> {
        let path = self.fingerprint_to_path_published(fpr);
        self.read_from_path(&path, false)
    }

    // XXX: slow
    fn by_fpr(&self, fpr: &Fingerprint) -> Option<String> {
        let path = self.link_by_fingerprint(fpr);
        self.read_from_path(&path, false)
    }

    // XXX: slow
    fn by_email(&self, email: &Email) -> Option<String> {
        let path = self.link_by_email(&email);
        self.read_from_path(&path, false)
    }

    // XXX: slow
    fn by_kid(&self, kid: &KeyID) -> Option<String> {
        let path = self.link_by_keyid(kid);
        self.read_from_path(&path, false)
    }

    /// Checks the database for consistency.
    ///
    /// Note that this operation may take a long time, and is
    /// generally only useful for testing.
    fn check_consistency(&self) -> Result<()> {
        use std::collections::HashMap;
        use failure::format_err;

        // A cache of all TPKs, for quick lookups.
        let mut tpks = HashMap::new();

        self.perform_checks(&self.keys_dir_published, &mut tpks,
            |path, _, primary_fp| {
                // The KeyID corresponding with this path.
                let fp = Filesystem::path_to_fingerprint(&path)
                    .ok_or_else(|| format_err!("Malformed path: {:?}", path))?;

                if fp != *primary_fp {
                    return Err(format_err!(
                        "{:?} points to the wrong TPK, expected {} \
                            but found {}",
                        path, fp, primary_fp));
                }
                Ok(())
            }
        )?;

        // check that all subkeys are linked
        self.perform_checks(&self.keys_dir_published, &mut tpks,
            |_, tpk, primary_fp| {
                let fingerprints = tpk
                    .keys_all()
                    .certification_capable()
                    .signing_capable()
                    .map(|(_, _, key)| key.fingerprint())
                    .map(|fpr| Fingerprint::try_from(fpr))
                    .flatten();

                for fpr in fingerprints {
                    if let Some(missing_fpr) = self.check_link_fpr(&fpr, &primary_fp)? {
                        return Err(format_err!(
                            "Missing link to key {} for sub {}", primary_fp, missing_fpr));
                    }
                }
                Ok(())
            }
        )?;

        // check that all published uids are linked
        self.perform_checks(&self.keys_dir_published, &mut tpks,
            |_, tpk, primary_fp| {
                let emails = tpk
                    .userids()
                    .map(|binding| binding.userid().clone())
                    .map(|userid| Email::try_from(&userid).unwrap());

                for email in emails {
                    let email_path = self.link_by_email(&email);
                    if !email_path.exists() {
                        return Err(format_err!(
                            "Missing link to key {} for email {}", primary_fp, email));
                    }
                }
                Ok(())
            }
        )?;


        self.perform_checks(&self.links_dir_by_fingerprint, &mut tpks,
            |path, tpk, _| {
                // The KeyID corresponding with this path.
                let id = Filesystem::path_to_keyid(&path)
                    .ok_or_else(|| format_err!("Malformed path: {:?}", path))?;

                let found = tpk.keys_all()
                    .map(|(_, _, key)| KeyID::try_from(key.fingerprint()).unwrap())
                    .any(|key_fp| key_fp == id);
                if ! found {
                    return Err(format_err!(
                        "{:?} points to the wrong TPK, the TPK does not \
                            contain the (sub)key {}", path, id));
                }
                Ok(())
            }
        )?;

        self.perform_checks(&self.links_dir_by_keyid, &mut tpks,
            |path, tpk, _| {
                // The KeyID corresponding with this path.
                let id = Filesystem::path_to_keyid(&path)
                    .ok_or_else(|| format_err!("Malformed path: {:?}", path))?;

                let found = tpk.keys_all()
                    .map(|(_, _, key)| KeyID::try_from(key.fingerprint()).unwrap())
                    .any(|key_fp| key_fp == id);
                if ! found {
                    return Err(format_err!(
                        "{:?} points to the wrong TPK, the TPK does not \
                            contain the (sub)key {}", path, id));
                }
                Ok(())
            }
        )?;

        self.perform_checks(&self.links_dir_by_email, &mut tpks,
            |path, tpk, _| {
                // The Email corresponding with this path.
                let email = Filesystem::path_to_email(&path)
                    .ok_or_else(|| format_err!("Malformed path: {:?}", path))?;
                let mut found = false;
                for uidb in tpk.userids() {
                    if Email::try_from(uidb.userid()).unwrap() == email
                    {
                        found = true;
                        break;
                    }
                }
                if ! found {
                    return Err(format_err!(
                        "{:?} points to the wrong TPK, the TPK does not \
                            contain the email {}", path, email));
                }
                Ok(())
        })?;

        Ok(())
    }
}

fn path_split(path: &str) -> PathBuf {
    if path.len() > 4 {
        [&path[..2], &path[2..4], &path[4..]].iter().collect()
    } else {
        path.into()
    }
}

fn path_merge(path: &Path) -> String {
    let comps = path.iter().rev().take(3).collect::<Vec<_>>().into_iter().rev();
    let comps: Vec<_> = comps.map(|os| os.to_string_lossy()).collect();
    comps.join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use test;
    use openpgp::tpk::TPKBuilder;
    use tempfile::TempDir;

    #[test]
    fn init() {
        let tmpdir = TempDir::new().unwrap();
        let _ = Filesystem::new_from_base(tmpdir.path()).unwrap();
    }

    #[test]
    fn new() {
        let tmpdir = TempDir::new().unwrap();
        let db = Filesystem::new_from_base(tmpdir.path()).unwrap();
        let k1 = TPKBuilder::new().add_userid("a@invalid.example.org")
            .generate().unwrap().0;
        let k2 = TPKBuilder::new().add_userid("b@invalid.example.org")
            .generate().unwrap().0;
        let k3 = TPKBuilder::new().add_userid("c@invalid.example.org")
            .generate().unwrap().0;

        assert!(db.merge(k1).unwrap().into_tpk_status().email_status.len() > 0);
        assert!(db.merge(k2.clone()).unwrap().into_tpk_status().email_status.len() > 0);
        assert!(!db.merge(k2).unwrap().into_tpk_status().email_status.len() > 0);
        assert!(db.merge(k3.clone()).unwrap().into_tpk_status().email_status.len() > 0);
        assert!(!db.merge(k3.clone()).unwrap().into_tpk_status().email_status.len() > 0);
        assert!(!db.merge(k3).unwrap().into_tpk_status().email_status.len() > 0);
    }

    #[test]
    fn uid_verification() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_uid_verification(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn uid_deletion() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_uid_deletion(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn subkey_lookup() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_subkey_lookup(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn kid_lookup() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_kid_lookup(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn upload_revoked_tpk() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();
        test::test_upload_revoked_tpk(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn uid_revocation() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_uid_revocation(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn regenerate() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_regenerate(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn key_reupload() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_reupload(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn uid_replacement() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_uid_replacement(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn uid_unlinking() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();
        test::test_unlink_uid(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn same_email_1() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_same_email_1(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn same_email_2() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_same_email_2(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn bad_uids() {
        let tmpdir = TempDir::new().unwrap();
        let mut db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        test::test_bad_uids(&mut db);
        db.check_consistency().expect("inconsistent database");
    }

    #[test]
    fn reverse_fingerprint_to_path() {
        let tmpdir = TempDir::new().unwrap();
        let db = Filesystem::new_from_base(tmpdir.path()).unwrap();

        let fp: Fingerprint =
            "CBCD8F030588653EEDD7E2659B7DD433F254904A".parse().unwrap();

        assert_eq!(Filesystem::path_to_fingerprint(&db.link_by_fingerprint(&fp)),
                   Some(fp.clone()));
        db.check_consistency().expect("inconsistent database");
    }
}
