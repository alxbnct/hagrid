use rocket;
use rocket::fairing::AdHoc;
use rocket::http::{Header, Status};
use rocket::request::{self, FromRequest, Request};
use rocket::response::status::Custom;
use rocket::response::NamedFile;
use rocket::{Outcome, State};
use rocket_contrib::templates::Template;
use rocket::request::Form;

use serde::Serialize;
use handlebars::Handlebars;

use std::path::{Path, PathBuf};

mod upload;
use mail;

use database::{Database, Polymorphic, Query};
use Result;
use types::{Email, Fingerprint, KeyID};
use Opt;

use std::result;
use std::str::FromStr;

mod queries {
    use std::fmt;
    use types::{Email, Fingerprint, KeyID};

    #[derive(Debug)]
    pub enum Hkp {
        Fingerprint { fpr: Fingerprint, index: bool, machine_readable: bool },
        KeyID { keyid: KeyID, index: bool, machine_readable: bool },
        Email { email: Email, index: bool },
        Invalid{ query: String, },
    }

    impl fmt::Display for Hkp {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match self {
                Hkp::Fingerprint{ ref fpr,.. } => write!(f, "{}", fpr.to_string()),
                Hkp::KeyID{ ref keyid,.. } => write!(f, "{}", keyid.to_string()),
                Hkp::Email{ ref email,.. } => write!(f, "{}", email.to_string()),
                Hkp::Invalid{ ref query } => write!(f, "{}", query),
            }
        }
    }
}

use rocket::http::hyper::header::ContentDisposition;

#[derive(Responder)]
enum MyResponse {
    #[response(status = 200, content_type = "html")]
    Success(Template),
     #[response(status = 200, content_type = "plain")]
    Plain(String),
     #[response(status = 200, content_type = "application/pgp-keys")]
    Key(String, ContentDisposition),
    #[response(status = 200, content_type = "application/pgp-keys")]
    XAccelRedirect(&'static str, Header<'static>, ContentDisposition),
    #[response(status = 500, content_type = "html")]
    ServerError(Template),
    #[response(status = 404, content_type = "html")]
    NotFound(Template),
}

impl MyResponse {
    pub fn ok<S: Serialize>(tmpl: &'static str, ctx: S) -> Self {
        MyResponse::Success(Template::render(tmpl, ctx))
    }

    pub fn plain(s: String) -> Self {
        MyResponse::Plain(s)
    }

    pub fn key(armored_key: String, fp: &Fingerprint) -> Self {
        use rocket::http::hyper::header::{ContentDisposition, DispositionType,
                                          DispositionParam, Charset};
        MyResponse::Key(
            armored_key,
            ContentDisposition {
                disposition: DispositionType::Attachment,
                parameters: vec![
                    DispositionParam::Filename(
                        Charset::Us_Ascii, None,
                        (fp.to_string() + ".asc").into_bytes()),
                ],
            })
    }

    pub fn x_accel_redirect(path: PathBuf, fp: &Fingerprint) -> Self {
        use rocket::http::hyper::header::{ContentDisposition, DispositionType,
                                          DispositionParam, Charset};
        // The path is relative to our base directory, but we need to
        // get it relative to base/public.
        let mut path = path.into_os_string().into_string().expect("valid UTF8");
        // Drop the first component.
        assert!(path.starts_with("public/"));
        path.drain(..6);

        MyResponse::XAccelRedirect(
            "",
            Header::new("X-Accel-Redirect", path),
            ContentDisposition {
                disposition: DispositionType::Attachment,
                parameters: vec![
                    DispositionParam::Filename(
                        Charset::Us_Ascii, None,
                        (fp.to_string() + ".asc").into_bytes()),
                ],
            })
    }

    pub fn ise(e: failure::Error) -> Self {
        let ctx = templates::FiveHundred{
            error: format!("{}", e),
            version: env!("VERGEN_SEMVER").to_string(),
            commit: env!("VERGEN_SHA_SHORT").to_string(),
        };
        MyResponse::ServerError(Template::render("500", ctx))
    }

    pub fn not_found<M>(tmpl: Option<&'static str>, message: M)
                        -> Self
        where M: Into<Option<String>>,
    {
        MyResponse::NotFound(
            Template::render(
                tmpl.unwrap_or("index"),
                templates::Index::new(
                    Some(message.into()
                         .unwrap_or_else(|| "Key not found".to_owned())))))
    }
}

mod templates {
    #[derive(Serialize)]
    pub struct Verify {
        pub verified: bool,
        pub userid: String,
        pub fpr: String,
        pub domain: String,
        pub commit: String,
        pub version: String,
    }

    #[derive(Serialize)]
    pub struct Delete {
        pub token: String,
        pub fpr: String,
        pub commit: String,
        pub version: String,
    }

    #[derive(Serialize)]
    pub struct Search {
        pub query: String,
        pub fpr: Option<String>,
        pub domain: Option<String>,
        pub commit: String,
        pub version: String,
    }

    #[derive(Serialize)]
    pub struct Confirm {
        pub deleted: bool,
        pub commit: String,
        pub version: String,
    }

    #[derive(Serialize)]
    pub struct FiveHundred {
        pub error: String,
        pub commit: String,
        pub version: String,
    }

    #[derive(Serialize)]
    pub struct Index {
        pub error: Option<String>,
        pub commit: String,
        pub version: String,
    }

    impl Index {
        pub fn new(error: Option<String>) -> Self {
            Self {
                error: error,
                version: env!("VERGEN_SEMVER").to_string(),
                commit: env!("VERGEN_SHA_SHORT").to_string(),
            }
        }
    }

    #[derive(Serialize)]
    pub struct General {
        pub commit: String,
        pub version: String,
    }
}

struct StaticDir(String);
pub struct Domain(String);
pub struct XAccelRedirect(bool);

impl<'a, 'r> FromRequest<'a, 'r> for queries::Hkp {
    type Error = ();

    fn from_request(
        request: &'a Request<'r>,
    ) -> request::Outcome<queries::Hkp, ()> {
        use rocket::request::FormItems;
        use std::collections::HashMap;

        let query = request.uri().query().unwrap_or("");
        let fields = FormItems::from(query)
            .map(|item| {
                let (k, v) = item.key_value();

                let key = k.url_decode().unwrap_or_default();
                let value = v.url_decode().unwrap_or_default();
                (key, value)
            })
            .collect::<HashMap<_, _>>();

        if fields.len() >= 2
            && fields
                .get("op")
                .map(|x| x == "get" || x == "index")
                .unwrap_or(false)
        {
            let index = fields.get("op").map(|x| x == "index").unwrap_or(false);
            let machine_readable =
                fields.get("options").map(|x| x.contains("mr"))
                .unwrap_or(false);
            let search = fields.get("search").cloned().unwrap_or_default();
            let maybe_fpr = Fingerprint::from_str(&search);
            let maybe_keyid = KeyID::from_str(&search);

            if let Ok(fpr) = maybe_fpr {
                Outcome::Success(queries::Hkp::Fingerprint {
                    fpr: fpr,
                    index: index,
                    machine_readable: machine_readable,
                })
            } else if let Ok(keyid) = maybe_keyid {
                Outcome::Success(queries::Hkp::KeyID {
                    keyid: keyid,
                    index: index,
                    machine_readable: machine_readable,
                })
            } else {
                match Email::from_str(&search) {
                    Ok(email) => {
                        Outcome::Success(queries::Hkp::Email {
                            email: email,
                            index: index,
                        })
                    }
                    Err(_) => {
                        Outcome::Success(queries::Hkp::Invalid{
                            query: search
                        })
                    }
                }
            }
        } else {
            Outcome::Failure((Status::BadRequest, ()))
        }
    }
}

fn key_to_response<'a>(db: rocket::State<Polymorphic>,
                       query_string: String, domain: String,
                       query: Query,
                       machine_readable: bool,
                       x_accel_redirect: rocket::State<XAccelRedirect>)
                       -> MyResponse {
    let fp = if let Some(fp) = db.lookup_primary_fingerprint(&query) {
        fp
    } else {
        return MyResponse::not_found(None, None);
    };

    if machine_readable {
        if x_accel_redirect.0 {
            if let Some(path) = db.lookup_path(&query) {
                return MyResponse::x_accel_redirect(path, &fp);
            }
        }

        return match db.by_fpr(&fp) {
            Some(armored) => MyResponse::key(armored, &fp.into()),
            None => MyResponse::not_found(None, None),
        }
    }

    let context = templates::Search{
        query: query_string,
        domain: Some(domain),
        fpr: fp.to_string().into(),
        version: env!("VERGEN_SEMVER").to_string(),
        commit: env!("VERGEN_SHA_SHORT").to_string(),
    };

    MyResponse::ok("found", context)
}

fn key_to_hkp_index<'a>(db: rocket::State<Polymorphic>, query: Query)
                        -> MyResponse {
    use sequoia_openpgp::RevocationStatus;

    let tpk = match db.lookup(&query) {
        Ok(Some(tpk)) => tpk,
        Ok(None) => return MyResponse::not_found(None, None),
        Err(err) => { return MyResponse::ise(err); }
    };
    let mut out = String::default();
    let p = tpk.primary();

    let ctime = tpk
        .primary_key_signature()
        .and_then(|x| x.signature_creation_time())
        .map(|x| format!("{}", x.to_timespec().sec))
        .unwrap_or_default();
    let extime = tpk
        .primary_key_signature()
        .and_then(|x| x.signature_expiration_time())
        .map(|x| format!("{}", x))
        .unwrap_or_default();
    let is_exp = tpk
        .primary_key_signature()
        .and_then(|x| {
            if x.signature_expired() { "e" } else { "" }.into()
        })
    .unwrap_or_default();
    let is_rev =
        if tpk.revoked(None) != RevocationStatus::NotAsFarAsWeKnow {
            "r"
        } else {
            ""
        };
    let algo: u8 = p.pk_algo().into();

    out.push_str("info:1:1\r\n");
    out.push_str(&format!(
            "pub:{}:{}:{}:{}:{}:{}{}\r\n",
            p.fingerprint().to_string().replace(" ", ""),
            algo,
            p.mpis().bits(),
            ctime,
            extime,
            is_exp,
            is_rev
    ));

    for uid in tpk.userids() {
        let u =
            url::form_urlencoded::byte_serialize(uid.userid().userid())
            .fold(String::default(), |acc, x| acc + x);
        let ctime = uid
            .binding_signature()
            .and_then(|x| x.signature_creation_time())
            .map(|x| format!("{}", x.to_timespec().sec))
            .unwrap_or_default();
        let extime = uid
            .binding_signature()
            .and_then(|x| x.signature_expiration_time())
            .map(|x| format!("{}", x))
            .unwrap_or_default();
        let is_exp = uid
            .binding_signature()
            .and_then(|x| {
                if x.signature_expired() { "e" } else { "" }.into()
            })
        .unwrap_or_default();
        let is_rev = if uid.revoked(None)
            != RevocationStatus::NotAsFarAsWeKnow
            {
                "r"
            } else {
                ""
            };

        out.push_str(&format!(
                "uid:{}:{}:{}:{}{}\r\n",
                u, ctime, extime, is_exp, is_rev
        ));
    }

    MyResponse::plain(out)

}

#[get("/vks/v1/by-fingerprint/<fpr>")]
fn by_fingerprint(db: rocket::State<Polymorphic>, domain: rocket::State<Domain>,
                  x_accel_redirect: rocket::State<XAccelRedirect>,
                  fpr: String) -> MyResponse {
    let query = match Fingerprint::from_str(&fpr) {
        Ok(fpr) => Query::ByFingerprint(fpr),
        Err(e) => return MyResponse::ise(e),
    };

    key_to_response(db, fpr, domain.0.clone(), query, true, x_accel_redirect)
}

#[get("/vks/v1/by-email/<email>")]
fn by_email(db: rocket::State<Polymorphic>, domain: rocket::State<Domain>,
            x_accel_redirect: rocket::State<XAccelRedirect>,
            email: String) -> MyResponse {
    let query = match Email::from_str(&email) {
        Ok(email) => Query::ByEmail(email),
        Err(e) => return MyResponse::ise(e),
    };

    key_to_response(db, email, domain.0.clone(), query, true, x_accel_redirect)
}

#[get("/vks/v1/by-keyid/<kid>")]
fn by_keyid(db: rocket::State<Polymorphic>, domain: rocket::State<Domain>,
            x_accel_redirect: rocket::State<XAccelRedirect>,
            kid: String) -> MyResponse {
    let query = match KeyID::from_str(&kid) {
        Ok(keyid) => Query::ByKeyID(keyid),
        Err(e) => return MyResponse::ise(e),
    };

    key_to_response(db, kid, domain.0.clone(), query, true, x_accel_redirect)
}

#[get("/vks/v1/verify/<token>")]
fn verify(
    db: rocket::State<Polymorphic>, domain: rocket::State<Domain>, token: String,
) -> result::Result<Template, Custom<String>> {
    match db.verify_token(&token) {
        Ok(Some((userid, fpr))) => {
            let context = templates::Verify {
                verified: true,
                domain: domain.0.clone(),
                userid: userid.to_string(),
                fpr: fpr.to_string(),
                version: env!("VERGEN_SEMVER").to_string(),
                commit: env!("VERGEN_SHA_SHORT").to_string(),
            };

            Ok(Template::render("verify", context))
        }
        Ok(None) | Err(_) => {
            let context = templates::Verify {
                verified: false,
                domain: domain.0.clone(),
                userid: "".into(),
                fpr: "".into(),
                version: env!("VERGEN_SEMVER").to_string(),
                commit: env!("VERGEN_SHA_SHORT").to_string(),
            };

            Ok(Template::render("verify", context))
        }
    }
}

#[get("/vks/v1/manage")]
fn manage() -> result::Result<Template, Custom<String>> {
    Ok(Template::render("manage", templates::Index::new(None)))
}

#[derive(FromForm)]
struct ManageRequest {
    search_term: String,
}

#[post("/vks/v1/manage", data="<request>")]
fn manage_post(
    db: State<Polymorphic>, mail_service: State<mail::Service>,
    domain: State<Domain>, request: Form<ManageRequest>,
) -> MyResponse {
    use std::convert::TryInto;

    let query = match request.search_term.parse() {
        Ok(query) => query,
        Err(e) => return MyResponse::ise(e),
    };
    let tpk = match db.lookup(&query) {
        Ok(Some(tpk)) => tpk,
        Ok(None) => return MyResponse::not_found(
            Some("manage"),
            Some(format!("No such key found for {:?}", request.search_term))),
        Err(e) => return MyResponse::ise(e),
    };

    match db.request_deletion(tpk.fingerprint().try_into().unwrap()) {
        Ok((token, uids)) => {
            let context = templates::Delete {
                fpr: tpk.fingerprint().to_string(),
                token: token.clone(),
                version: env!("VERGEN_SEMVER").to_string(),
                commit: env!("VERGEN_SHA_SHORT").to_string(),
            };

            for uid in uids {
                if let Err(e) = mail_service.send_confirmation(
                    &uid, &token, &domain.0) {
                    return MyResponse::ise(e);
                }
            }

            MyResponse::ok("delete", context)
        }
        Err(e) => MyResponse::ise(e),
    }
}

#[get("/vks/v1/confirm/<token>")]
fn confirm(
    db: rocket::State<Polymorphic>, token: String,
) -> result::Result<Template, Custom<String>> {
    match db.confirm_deletion(&token) {
        Ok(true) => {
            let context = templates::Confirm {
                deleted: true,
                version: env!("VERGEN_SEMVER").to_string(),
                commit: env!("VERGEN_SHA_SHORT").to_string(),
            };

            Ok(Template::render("confirm", context))
        }
        Ok(false) | Err(_) => {
            let context = templates::Confirm {
                deleted: false,
                version: env!("VERGEN_SEMVER").to_string(),
                commit: env!("VERGEN_SHA_SHORT").to_string(),
            };

            Ok(Template::render("confirm", context))
        }
    }
}

#[get("/assets/<file..>")]
fn files(file: PathBuf, static_dir: State<StaticDir>) -> Option<NamedFile> {
    NamedFile::open(Path::new(&static_dir.0).join("assets").join(file)).ok()
}

#[get("/pks/lookup")]
fn lookup(db: rocket::State<Polymorphic>, domain: rocket::State<Domain>,
          x_accel_redirect: rocket::State<XAccelRedirect>,
          key: Option<queries::Hkp>) -> MyResponse {
    let query_string = key.as_ref().map(|k| format!("{}", k));
    let (query, index, machine_readable) = match key {
        Some(queries::Hkp::Fingerprint { fpr, index, machine_readable }) =>
            (Query::ByFingerprint(fpr), index, machine_readable),
        Some(queries::Hkp::KeyID { keyid, index, machine_readable }) =>
            (Query::ByKeyID(keyid), index, machine_readable),
        Some(queries::Hkp::Email { email, index }) => {
            // XXX: Maybe return 501 Not Implemented if machine_readable
            (Query::ByEmail(email), index, false)
        }
        Some(queries::Hkp::Invalid { query: _ }) => {
            return MyResponse::not_found(None, None);
        }
        None => {
            return MyResponse::not_found(None, None);
        }
    };

    if index {
        key_to_hkp_index(db, query)
    } else {
        key_to_response(db,
                        query_string.expect("key was Some if we made it here"),
                        domain.0.clone(), query, machine_readable,
                        x_accel_redirect)
    }
}

#[get("/")]
fn root() -> Template {
    Template::render("index", templates::Index::new(None))
}

#[get("/about")]
fn about() -> Template {
    let context = templates::General {
        version: env!("VERGEN_SEMVER").to_string(),
        commit: env!("VERGEN_SHA_SHORT").to_string(),
    };

    Template::render("about", context)
}

pub fn serve(opt: &Opt, db: Polymorphic) -> Result<()> {
    use rocket::config::{Config, Environment};
    use std::str::FromStr;

    let (addr, port) = match opt.listen.find(':') {
        Some(p) => {
            let addr = opt.listen[0..p].to_string();
            let port = if p < opt.listen.len() - 1 {
                u16::from_str(&opt.listen[p + 1..]).ok().unwrap_or(8080)
            } else {
                8080
            };

            (addr, port)
        }
        None => (opt.listen.to_string(), 8080),
    };

    let config = Config::build(Environment::Staging)
        .address(addr)
        .port(port)
        .workers(2)
        .root(opt.base.clone())
        .extra(
            "template_dir",
            opt.base
                .join("templates")
                .to_str()
                .ok_or(failure::err_msg("Template path invalid"))?,
        )
        .extra(
            "static_dir",
            opt.base.join("public").to_str()
                .ok_or(failure::err_msg("Static path invalid"))?,
        )
        .extra("domain", opt.domain.clone())
        .extra("from", opt.from.clone())
        .extra("x-accel-redirect", opt.x_accel_redirect)
        .finalize()?;

    rocket_factory(rocket::custom(config), db).launch();
    Ok(())
}

fn rocket_factory(rocket: rocket::Rocket, db: Polymorphic) -> rocket::Rocket {
    let routes = routes![
        // infra
        root,
        manage,
        manage_post,
        files,
        // nginx-supported lookup
        by_email,
        by_fingerprint,
        by_keyid,
        // HKP
        lookup,
        upload::vks_publish,
        upload::vks_publish_submit,
        // verification & deletion
        verify,
        confirm,
        // about
        about,
    ];

    rocket
        .attach(Template::fairing())
        .attach(AdHoc::on_attach("static_dir", |rocket| {
            let static_dir =
                rocket.config().get_str("static_dir").unwrap().to_string();

            Ok(rocket.manage(StaticDir(static_dir)))
        }))
        .attach(AdHoc::on_attach("domain", |rocket| {
            let domain = rocket.config().get_str("domain").unwrap().to_string();

            Ok(rocket.manage(Domain(domain)))
        }))
        .attach(AdHoc::on_attach("x-accel-redirect", |rocket| {
            let x_accel_redirect =
                rocket.config().get_bool("x-accel-redirect").unwrap();

            Ok(rocket.manage(XAccelRedirect(x_accel_redirect)))
        }))
        .attach(AdHoc::on_attach("mail-service", |rocket| {
            let dir: PathBuf = rocket
                .config()
                .get_str("template_dir")
                .unwrap()
                .to_string()
                .into();
            let from = rocket.config().get_str("from").unwrap().to_string();

            let confirm_html = dir.join("confirm-email-html.hbs");
            let confirm_txt = dir.join("confirm-email-txt.hbs");
            let verify_html = dir.join("verify-email-html.hbs");
            let verify_txt = dir.join("verify-email-txt.hbs");
            let mut handlebars = Handlebars::new();

            handlebars
                .register_template_file("confirm-html", confirm_html)
                .unwrap();
            handlebars
                .register_template_file("confirm-txt", confirm_txt)
                .unwrap();
            handlebars
                .register_template_file("verify-html", verify_html)
                .unwrap();
            handlebars
                .register_template_file("verify-txt", verify_txt)
                .unwrap();

            Ok(rocket.manage(mail::Service::sendmail(from, handlebars)))
        }))
        .mount("/", routes)
        .manage(db)
}

#[cfg(test)]
mod tests {
    use fs_extra;
    use tempfile::{tempdir, TempDir};
    use super::rocket;
    use rocket::local::Client;
    use rocket::http::Status;
    use rocket::http::ContentType;

    use sequoia_openpgp::TPK;
    use sequoia_openpgp::tpk::TPKBuilder;
    use sequoia_openpgp::parse::Parse;
    use sequoia_openpgp::serialize::Serialize;

    use database::*;
    use super::*;

    /// Creates a configuration and empty state dir for testing purposes.
    ///
    /// Note that you need to keep the returned TempDir alive for the
    /// duration of your test.  To debug the test, mem::forget it to
    /// prevent cleanup.
    fn configuration() -> Result<(TempDir, rocket::Config)> {
        use rocket::config::{Config, Environment};

        let root = tempdir()?;
        fs_extra::copy_items(&vec!["dist/templates"], &root,
                             &fs_extra::dir::CopyOptions::new())?;

        let config = Config::build(Environment::Staging)
            .root(root.path().to_path_buf())
            .extra(
                "template_dir",
                root.path().join("templates").to_str()
                    .ok_or(failure::err_msg("Template path invalid"))?,
            )
            .extra(
                "static_dir",
                root.path().join("public").to_str()
                    .ok_or(failure::err_msg("Static path invalid"))?,
            )
            .extra("domain", "domain")
            .extra("from", "from")
            .extra("x-accel-redirect", false)
            .finalize()?;
        Ok((root, config))
    }

    #[test]
    fn basics() {
        let (_tmpdir, config) = configuration().unwrap();

        let db = Polymorphic::Filesystem(
            Filesystem::new(config.root().unwrap().to_path_buf()).unwrap());
        let rocket = rocket_factory(rocket::custom(config), db);
        let client = Client::new(rocket).expect("valid rocket instance");

        // Check that we see the landing page.
        let mut response = client.get("/").dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.content_type(), Some(ContentType::HTML));
        assert!(response.body_string().unwrap().contains("Hagrid"));

        // Check that we see the privacy policy.
        let mut response = client.get("/about").dispatch();
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.content_type(), Some(ContentType::HTML));
        assert!(response.body_string().unwrap().contains("Public Key Data"));
    }

    #[test]
    fn upload() {
        let (_tmpdir, config) = configuration().unwrap();

        // eprintln!("LEAKING: {:?}", _tmpdir);
        // ::std::mem::forget(_tmpdir);

        let db = Polymorphic::Filesystem(
            Filesystem::new(config.root().unwrap().to_path_buf()).unwrap());
        let rocket = rocket_factory(rocket::custom(config), db);
        let client = Client::new(rocket).expect("valid rocket instance");

        // Generate a key and upload it.
        let (tpk, _) = TPKBuilder::autocrypt(
            None, Some("foo@invalid.example.com".into()))
            .generate().unwrap();
        let fp = tpk.fingerprint().to_hex();
        let keyid = tpk.fingerprint().to_keyid().to_hex();

        let mut tpk_serialized = Vec::new();
        tpk.serialize(&mut tpk_serialized).unwrap();
        let response = vks_publish_submit(&client, &tpk_serialized);
        assert_eq!(response.status(), Status::SeeOther);
        assert_eq!(response.headers().get_one("Location"),
                   Some("/vks/v1/publish?ok"));

        // And check that we can get it back, modulo user ids.
        fn check_mr_response(client: &Client, uri: &str, tpk: &TPK) {
            let mut response = client.get(uri).dispatch();
            assert_eq!(response.status(), Status::Ok);
            assert_eq!(response.content_type(),
                       Some(ContentType::new("application", "pgp-keys")));
            let body = response.body_string().unwrap();
            assert!(body.contains("END PGP PUBLIC KEY BLOCK"));
            let tpk_ = TPK::from_bytes(body.as_bytes()).unwrap();
            assert_eq!(tpk.fingerprint(), tpk_.fingerprint());
            assert_eq!(tpk.subkeys().map(|skb| skb.subkey().fingerprint())
                       .collect::<Vec<_>>(),
                       tpk_.subkeys().map(|skb| skb.subkey().fingerprint())
                       .collect::<Vec<_>>());
            assert_eq!(tpk_.userids().count(), 0);
        }

        check_mr_response(&client, &format!("/vks/v1/by-keyid/{}", keyid), &tpk);
        check_mr_response(&client, &format!("/vks/v1/by-fingerprint/{}", fp), &tpk);
        check_mr_response(
            &client,
            &format!("/pks/lookup?op=get&options=mr&search={}", fp),
            &tpk);
        check_mr_response(
            &client,
            &format!("/pks/lookup?op=get&options=mr&search=0x{}", fp),
            &tpk);
        check_mr_response(
            &client,
            &format!("/pks/lookup?op=get&options=mr&search={}", keyid),
            &tpk);
        check_mr_response(
            &client,
            &format!("/pks/lookup?op=get&options=mr&search=0x{}", keyid),
            &tpk);

        // And check that we can see the human-readable result page.
        fn check_hr_response(client: &Client, uri: &str, tpk: &TPK) {
            let mut response = client.get(uri).dispatch();
            assert_eq!(response.status(), Status::Ok);
            assert_eq!(response.content_type(), Some(ContentType::HTML));
            let body = response.body_string().unwrap();
            assert!(body.contains("found"));
            assert!(body.contains(&tpk.fingerprint().to_hex()));
        }

        check_hr_response(
            &client,
            &format!("/pks/lookup?op=get&search={}", fp),
            &tpk);
        check_hr_response(
            &client,
            &format!("/pks/lookup?op=get&search=0x{}", fp),
            &tpk);
        check_hr_response(
            &client,
            &format!("/pks/lookup?op=get&search={}", keyid),
            &tpk);
        check_hr_response(
            &client,
            &format!("/pks/lookup?op=get&search=0x{}", keyid),
            &tpk);
    }

    fn vks_publish_submit<'a>(client: &'a Client, data: &[u8])
                              -> rocket::local::LocalResponse<'a> {
        let ct = ContentType::with_params(
            "multipart", "form-data",
            ("boundary", "---------------------------14733842173518794281682249499"));

        let header =
            b"-----------------------------14733842173518794281682249499\r\n\
              Content-Disposition: form-data; name=\"csrf\"\r\n\
              \r\n\
              \r\n\
              -----------------------------14733842173518794281682249499\r\n\
              Content-Disposition: form-data; name=\"keytext\"; filename=\".k\"\r\n\
              Content-Type: application/octet-stream\r\n\
              \r\n";
        let footer = b"\r\n-----------------------------14733842173518794281682249499--";

        let mut body = Vec::new();
        body.extend_from_slice(header);
        body.extend_from_slice(data);
        body.extend_from_slice(footer);
        client.post("/vks/v1/publish/submit")
            .header(ct)
            .body(&body[..])
            .dispatch()
    }
}
