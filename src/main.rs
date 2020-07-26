use serde_derive::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use trout::hyper::RoutingFailureExtHyper;

mod apub_util;
mod routes;
mod tasks;
mod worker;

pub type DbPool = deadpool_postgres::Pool;
pub type HttpClient = hyper::Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>;

pub struct BaseContext {
    pub db_pool: DbPool,
    pub host_url_api: String,
    pub host_url_apub: String,
    pub http_client: HttpClient,
    pub apub_proxy_rewrites: bool,

    pub local_hostname: String,
}

pub struct RouteContext {
    base: Arc<BaseContext>,
    worker_trigger: tokio::sync::mpsc::Sender<()>,
}

impl RouteContext {
    pub async fn enqueue_task<T: crate::tasks::TaskDef>(
        &self,
        task: &T,
    ) -> Result<(), crate::Error> {
        let db = self.db_pool.get().await?;
        db.execute(
            "INSERT INTO task (kind, params, max_attempts, created_at) VALUES ($1, $2, $3, current_timestamp)",
            &[&T::KIND, &tokio_postgres::types::Json(task), &T::MAX_ATTEMPTS],
        ).await?;

        match self.worker_trigger.clone().try_send(()) {
            Ok(_) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                Err(crate::Error::InternalStrStatic("Worker channel closed"))
            }
        }
    }
}

impl std::ops::Deref for RouteContext {
    type Target = BaseContext;

    fn deref(&self) -> &BaseContext {
        &self.base
    }
}

pub type RouteNode<P> = trout::Node<
    P,
    hyper::Request<hyper::Body>,
    std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<hyper::Response<hyper::Body>, Error>> + Send>,
    >,
    Arc<RouteContext>,
>;

#[derive(Debug)]
pub enum Error {
    Internal(Box<dyn std::error::Error + Send>),
    InternalStr(String),
    InternalStrStatic(&'static str),
    UserError(hyper::Response<hyper::Body>),
    RoutingError(trout::RoutingFailure),
}

impl<T: 'static + std::error::Error + Send> From<T> for Error {
    fn from(err: T) -> Error {
        Error::Internal(Box::new(err))
    }
}

#[derive(Debug, PartialEq)]
pub enum APIDOrLocal {
    Local,
    APID(String),
}

pub enum TimestampOrLatest {
    Latest,
    Timestamp(chrono::DateTime<chrono::offset::FixedOffset>),
}

impl std::fmt::Display for TimestampOrLatest {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            TimestampOrLatest::Latest => write!(f, "latest"),
            TimestampOrLatest::Timestamp(ts) => write!(f, "{}", ts.timestamp()),
        }
    }
}

pub enum TimestampOrLatestParseError {
    Number(std::num::ParseIntError),
    Timestamp,
}

impl std::str::FromStr for TimestampOrLatest {
    type Err = TimestampOrLatestParseError;

    fn from_str(src: &str) -> Result<Self, Self::Err> {
        if src == "latest" {
            Ok(TimestampOrLatest::Latest)
        } else {
            use chrono::offset::TimeZone;

            let ts = src.parse().map_err(TimestampOrLatestParseError::Number)?;
            let ts = chrono::offset::Utc
                .timestamp_opt(ts, 0)
                .single()
                .ok_or(TimestampOrLatestParseError::Timestamp)?;
            Ok(TimestampOrLatest::Timestamp(ts.into()))
        }
    }
}

macro_rules! id_wrapper {
    ($ty:ident) => {
        #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
        #[serde(transparent)]
        pub struct $ty(pub i64);
        impl $ty {
            pub fn raw(&self) -> i64 {
                self.0
            }
        }
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl std::str::FromStr for $ty {
            type Err = std::num::ParseIntError;
            fn from_str(src: &str) -> Result<Self, Self::Err> {
                Ok(Self(src.parse()?))
            }
        }
        impl postgres_types::ToSql for $ty {
            fn to_sql(
                &self,
                ty: &postgres_types::Type,
                out: &mut bytes::BytesMut,
            ) -> Result<postgres_types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
                self.0.to_sql(ty, out)
            }
            fn accepts(ty: &postgres_types::Type) -> bool {
                i64::accepts(ty)
            }
            fn to_sql_checked(
                &self,
                ty: &postgres_types::Type,
                out: &mut bytes::BytesMut,
            ) -> Result<postgres_types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
                self.0.to_sql_checked(ty, out)
            }
        }
    };
}

id_wrapper!(CommentLocalID);
id_wrapper!(CommunityLocalID);
id_wrapper!(PostLocalID);
id_wrapper!(UserLocalID);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActorLocalRef {
    Person(UserLocalID),
    Community(CommunityLocalID),
}

pub enum ThingLocalRef {
    Post(PostLocalID),
    Comment(CommentLocalID),
}

#[derive(Debug)]
pub struct PostInfo<'a> {
    id: PostLocalID,
    author: Option<UserLocalID>,
    href: Option<&'a str>,
    content_text: Option<&'a str>,
    #[allow(dead_code)]
    content_markdown: Option<&'a str>,
    content_html: Option<&'a str>,
    title: &'a str,
    created: &'a chrono::DateTime<chrono::FixedOffset>,
    #[allow(dead_code)]
    community: CommunityLocalID,
}

pub struct PostInfoOwned {
    id: PostLocalID,
    author: Option<UserLocalID>,
    href: Option<String>,
    content_text: Option<String>,
    content_markdown: Option<String>,
    content_html: Option<String>,
    title: String,
    created: chrono::DateTime<chrono::FixedOffset>,
    community: CommunityLocalID,
}

impl<'a> Into<PostInfo<'a>> for &'a PostInfoOwned {
    fn into(self) -> PostInfo<'a> {
        PostInfo {
            id: self.id,
            author: self.author,
            href: self.href.as_deref(),
            content_text: self.content_text.as_deref(),
            content_markdown: self.content_markdown.as_deref(),
            content_html: self.content_html.as_deref(),
            title: &self.title,
            created: &self.created,
            community: self.community,
        }
    }
}

#[derive(Debug)]
pub struct CommentInfo<'a> {
    id: CommentLocalID,
    author: Option<UserLocalID>,
    post: PostLocalID,
    parent: Option<CommentLocalID>,
    content_text: Option<Cow<'a, str>>,
    #[allow(dead_code)]
    content_markdown: Option<Cow<'a, str>>,
    content_html: Option<Cow<'a, str>>,
    created: chrono::DateTime<chrono::FixedOffset>,
    ap_id: APIDOrLocal,
}

pub const KEY_BITS: u32 = 2048;

pub fn get_url_host(url: &str) -> Option<String> {
    url::Url::parse(url).ok().and_then(|url| {
        url.host_str().map(|host| match url.port() {
            Some(port) => format!("{}:{}", host, port),
            None => host.to_owned(),
        })
    })
}

pub fn get_actor_host<'a>(
    local: bool,
    ap_id: Option<&str>,
    local_hostname: &'a str,
) -> Option<Cow<'a, str>> {
    if local {
        Some(local_hostname.into())
    } else {
        ap_id.and_then(get_url_host).map(Cow::from)
    }
}

pub fn get_actor_host_or_unknown<'a>(
    local: bool,
    ap_id: Option<&str>,
    local_hostname: &'a str,
) -> Cow<'a, str> {
    get_actor_host(local, ap_id, local_hostname).unwrap_or(Cow::Borrowed("[unknown]"))
}

pub fn get_path_and_query(url: &str) -> Result<String, url::ParseError> {
    let url = url::Url::parse(&url)?;
    Ok(format!("{}{}", url.path(), url.query().unwrap_or("")))
}

pub async fn query_stream(
    db: &tokio_postgres::Client,
    statement: &(impl tokio_postgres::ToStatement + ?Sized),
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<tokio_postgres::RowStream, tokio_postgres::Error> {
    let params = params.iter().map(|s| *s as _);

    db.query_raw(statement, params).await
}

pub fn empty_response() -> hyper::Response<hyper::Body> {
    let mut res = hyper::Response::new((&[][..]).into());
    *res.status_mut() = hyper::StatusCode::NO_CONTENT;
    res
}

pub fn simple_response(
    code: hyper::StatusCode,
    text: impl Into<hyper::Body>,
) -> hyper::Response<hyper::Body> {
    let mut res = hyper::Response::new(text.into());
    *res.status_mut() = code;
    res
}

pub async fn res_to_error(
    res: hyper::Response<hyper::Body>,
) -> Result<hyper::Response<hyper::Body>, crate::Error> {
    if res.status().is_success() {
        Ok(res)
    } else {
        let bytes = hyper::body::to_bytes(res.into_body()).await?;
        Err(crate::Error::InternalStr(format!(
            "Error in remote response: {}",
            String::from_utf8_lossy(&bytes)
        )))
    }
}

lazy_static::lazy_static! {
    static ref LANG_MAP: HashMap<unic_langid::LanguageIdentifier, fluent::FluentResource> = {
        let mut result = HashMap::new();

        result.insert(unic_langid::langid!("en"), fluent::FluentResource::try_new(include_str!("../res/lang/en.ftl").to_owned()).expect("Failed to parse translation"));
        result.insert(unic_langid::langid!("eo"), fluent::FluentResource::try_new(include_str!("../res/lang/eo.ftl").to_owned()).expect("Failed to parse translation"));

        result
    };

    static ref LANGS: Vec<unic_langid::LanguageIdentifier> = {
        LANG_MAP.keys().cloned().collect()
    };
}

pub struct Translator {
    bundle: fluent::concurrent::FluentBundle<&'static fluent::FluentResource>,
}
impl Translator {
    pub fn tr<'a>(&'a self, key: &str, args: Option<&'a fluent::FluentArgs>) -> Cow<'a, str> {
        let mut errors = Vec::with_capacity(0);
        let out = self.bundle.format_pattern(
            self.bundle
                .get_message(key)
                .expect("Missing message in translation")
                .value
                .expect("Missing value for translation key"),
            args,
            &mut errors,
        );
        if !errors.is_empty() {
            eprintln!("Errors in translation: {:?}", errors);
        }

        out
    }
}

pub fn get_lang_for_req(req: &hyper::Request<hyper::Body>) -> Translator {
    let default = unic_langid::langid!("en");
    let languages = match req
        .headers()
        .get(hyper::header::ACCEPT_LANGUAGE)
        .and_then(|x| x.to_str().ok())
    {
        Some(accept_language) => {
            let requested = fluent_langneg::accepted_languages::parse(accept_language);
            fluent_langneg::negotiate_languages(
                &requested,
                &LANGS,
                Some(&default),
                fluent_langneg::NegotiationStrategy::Filtering,
            )
        }
        None => vec![&default],
    };

    let mut bundle = fluent::concurrent::FluentBundle::new(languages.iter().map(|x| *x));
    for lang in languages {
        if let Err(errors) = bundle.add_resource(&LANG_MAP[lang]) {
            for err in errors {
                match err {
                    fluent::FluentError::Overriding { .. } => {}
                    _ => {
                        eprintln!("Failed to add language resource: {:?}", err);
                        break;
                    }
                }
            }
        }
    }

    Translator { bundle }
}

pub async fn authenticate(
    req: &hyper::Request<hyper::Body>,
    db: &tokio_postgres::Client,
) -> Result<Option<UserLocalID>, Error> {
    use headers::Header;

    let value = match req.headers().get(hyper::header::AUTHORIZATION) {
        Some(value) => {
            match headers::Authorization::<headers::authorization::Bearer>::decode(
                &mut std::iter::once(value),
            ) {
                Ok(value) => value.0.token().to_owned(),
                Err(_) => return Ok(None),
            }
        }
        None => return Ok(None),
    };

    let token = match value.parse::<uuid::Uuid>() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    let row = db
        .query_opt("SELECT person FROM login WHERE token=$1", &[&token])
        .await?;

    match row {
        Some(row) => Ok(Some(UserLocalID(row.get(0)))),
        None => Ok(None),
    }
}

pub async fn require_login(
    req: &hyper::Request<hyper::Body>,
    db: &tokio_postgres::Client,
) -> Result<UserLocalID, Error> {
    authenticate(req, db).await?.ok_or_else(|| {
        Error::UserError(simple_response(
            hyper::StatusCode::UNAUTHORIZED,
            "Login Required",
        ))
    })
}

pub fn spawn_task<F: std::future::Future<Output = Result<(), Error>> + Send + 'static>(task: F) {
    use futures::future::TryFutureExt;
    tokio::spawn(task.map_err(|err| {
        eprintln!("Error in task: {:?}", err);
    }));
}

pub fn render_markdown(src: &str) -> String {
    let parser = pulldown_cmark::Parser::new(src);
    let mut output = String::new();
    pulldown_cmark::html::push_html(&mut output, parser);

    output
}

pub fn on_community_add_post(
    community: CommunityLocalID,
    post_local_id: PostLocalID,
    post_ap_id: &str,
    ctx: Arc<crate::RouteContext>,
) {
    println!("on_community_add_post");
    crate::apub_util::spawn_announce_community_post(community, post_local_id, post_ap_id, ctx);
}

pub fn on_community_add_comment(
    community: CommunityLocalID,
    comment_local_id: CommentLocalID,
    comment_ap_id: &str,
    ctx: Arc<crate::RouteContext>,
) {
    crate::apub_util::spawn_announce_community_comment(
        community,
        comment_local_id,
        comment_ap_id,
        ctx,
    );
}

pub fn on_post_add_comment(comment: CommentInfo<'static>, ctx: Arc<crate::RouteContext>) {
    println!("on_post_add_comment");
    spawn_task(async move {
        let db = ctx.db_pool.get().await?;

        let res = futures::future::try_join(
            db.query_opt(
                "SELECT community.id, community.local, community.ap_id, community.ap_inbox, post.local, post.ap_id, person.id, person.ap_id FROM community, post LEFT OUTER JOIN person ON (person.id = post.author) WHERE post.id = $1 AND post.community = community.id",
                &[&comment.post.raw()],
            ),
            async {
                match comment.parent {
                    Some(parent) => {
                        let row = db.query_one(
                            "SELECT reply.local, reply.ap_id, person.id, person.ap_id FROM reply LEFT OUTER JOIN person ON (person.id = reply.author) WHERE reply.id=$1",
                            &[&parent],
                        ).await?;

                        let author_local_id = row.get::<_, Option<_>>(2).map(UserLocalID);

                        if row.get(0) {
                            Ok(Some((crate::apub_util::get_local_comment_apub_id(parent, &ctx.host_url_apub), Some(crate::apub_util::get_local_person_apub_id(UserLocalID(row.get(2)), &ctx.host_url_apub)), true, author_local_id)))
                        } else {
                            Ok(row.get::<_, Option<String>>(1).map(|x| (x, row.get(3), false, author_local_id)))
                        }
                    },
                    None => Ok(None),
                }
            }
        ).await?;

        if let Some(row) = res.0 {
            let community_local: bool = row.get(1);
            let post_local: bool = row.get(4);

            let post_ap_id = if post_local {
                Some(crate::apub_util::get_local_post_apub_id(
                    comment.post,
                    &ctx.host_url_apub,
                ))
            } else {
                row.get(5)
            };

            let comment_ap_id = match &comment.ap_id {
                crate::APIDOrLocal::APID(apid) => Cow::Borrowed(apid),
                crate::APIDOrLocal::Local => Cow::Owned(
                    crate::apub_util::get_local_comment_apub_id(comment.id, &ctx.host_url_apub),
                ),
            };

            let (parent_ap_id, post_or_parent_author_local_id, post_or_parent_author_ap_id) =
                match comment.parent {
                    None => {
                        let author_id = UserLocalID(row.get(6));
                        if post_local {
                            (
                                None,
                                Some(author_id),
                                Some(Cow::<str>::Owned(
                                    crate::apub_util::get_local_person_apub_id(
                                        author_id,
                                        &ctx.host_url_apub,
                                    ),
                                )),
                            )
                        } else {
                            (
                                None,
                                Some(author_id),
                                row.get::<_, Option<_>>(7).map(Cow::Borrowed),
                            )
                        }
                    }
                    Some(_) => match &res.1 {
                        None => (None, None, None),
                        Some((parent_ap_id, parent_author_ap_id, _, parent_author_local_id)) => (
                            Some(parent_ap_id),
                            *parent_author_local_id,
                            parent_author_ap_id.as_deref().map(Cow::Borrowed),
                        ),
                    },
                };

            // Generate notifications
            match comment.parent {
                Some(parent_id) => {
                    if let Some((_, _, parent_local, parent_author_id)) = res.1 {
                        if parent_local && parent_author_id != comment.author {
                            if let Some(parent_author_id) = parent_author_id {
                                let ctx = ctx.clone();
                                let comment_id = comment.id;
                                crate::spawn_task(async move {
                                    let db = ctx.db_pool.get().await?;
                                    db.execute(
                                        "INSERT INTO notification (kind, created_at, to_user, reply, parent_reply) VALUES ('reply_reply', current_timestamp, $1, $2, $3)",
                                        &[&parent_author_id, &comment_id.raw(), &parent_id.raw()],
                                    ).await?;

                                    Ok(())
                                });
                            }
                        }
                    }
                }
                None => {
                    if post_local && post_or_parent_author_local_id != comment.author {
                        if let Some(post_or_parent_author_local_id) = post_or_parent_author_local_id
                        {
                            let ctx = ctx.clone();
                            let comment_id = comment.id;
                            let comment_post = comment.post;
                            crate::spawn_task(async move {
                                let db = ctx.db_pool.get().await?;
                                db.execute(
                                    "INSERT INTO notification (kind, created_at, to_user, reply, parent_post) VALUES ('post_reply', current_timestamp, $1, $2, $3)",
                                    &[&post_or_parent_author_local_id.raw(), &comment_id.raw(), &comment_post.raw()],
                                ).await?;

                                Ok(())
                            });
                        }
                    }
                }
            }

            if let Some(post_ap_id) = post_ap_id {
                if community_local {
                    let community = CommunityLocalID(row.get(0));
                    crate::on_community_add_comment(community, comment.id, &comment_ap_id, ctx);
                } else if comment.ap_id == APIDOrLocal::Local {
                    crate::apub_util::spawn_enqueue_send_comment_to_community(
                        comment,
                        row.get(2),
                        row.get(3),
                        post_ap_id,
                        parent_ap_id.cloned(),
                        post_or_parent_author_ap_id.as_deref(),
                        ctx,
                    );
                }
            }
        }

        Ok(())
    });
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host_url_apub =
        std::env::var("HOST_URL_ACTIVITYPUB").expect("Missing HOST_URL_ACTIVITYPUB");

    let host_url_api = std::env::var("HOST_URL_API").expect("Missing HOST_URL_API");

    let apub_proxy_rewrites = match std::env::var("APUB_PROXY_REWRITES") {
        Ok(value) => value.parse().expect("Failed to parse APUB_PROXY_REWRITES"),
        Err(std::env::VarError::NotPresent) => false,
        Err(other) => Err(other).expect("Failed to parse APUB_PROXY_REWRITES"),
    };

    let db_pool = deadpool_postgres::Pool::new(
        deadpool_postgres::Manager::new(
            std::env::var("DATABASE_URL")
                .expect("Missing DATABASE_URL")
                .parse()
                .unwrap(),
            tokio_postgres::NoTls,
        ),
        16,
    );

    let port = match std::env::var("PORT") {
        Ok(port_str) => port_str.parse().expect("Failed to parse port"),
        _ => 3333,
    };

    let routes = Arc::new(routes::route_root());
    let base_context = Arc::new(BaseContext {
        local_hostname: get_url_host(&host_url_apub).expect("Failed to parse HOST_URL_ACTIVITYPUB"),

        db_pool,
        host_url_api,
        host_url_apub,
        http_client: hyper::Client::builder().build(hyper_tls::HttpsConnector::new()),
        apub_proxy_rewrites,
    });

    let worker_trigger = worker::start_worker(base_context.clone());

    let context = Arc::new(RouteContext {
        base: base_context,
        worker_trigger,
    });

    let server = hyper::Server::bind(&(std::net::Ipv6Addr::UNSPECIFIED, port).into()).serve(
        hyper::service::make_service_fn(|_| {
            let routes = routes.clone();
            let context = context.clone();
            async {
                Ok::<_, hyper::Error>(hyper::service::service_fn(move |req| {
                    let routes = routes.clone();
                    let context = context.clone();
                    async move {
                        let result = match routes.route(req, context) {
                            Ok(fut) => fut.await,
                            Err(err) => Err(Error::RoutingError(err)),
                        };
                        Ok::<_, hyper::Error>(match result {
                            Ok(val) => val,
                            Err(Error::UserError(res)) => res,
                            Err(Error::RoutingError(err)) => err.to_simple_response(),
                            Err(Error::Internal(err)) => {
                                eprintln!("Error: {:?}", err);

                                simple_response(
                                    hyper::StatusCode::INTERNAL_SERVER_ERROR,
                                    "Internal Server Error",
                                )
                            }
                            Err(Error::InternalStr(err)) => {
                                eprintln!("Error: {}", err);

                                simple_response(
                                    hyper::StatusCode::INTERNAL_SERVER_ERROR,
                                    "Internal Server Error",
                                )
                            }
                            Err(Error::InternalStrStatic(err)) => {
                                eprintln!("Error: {}", err);

                                simple_response(
                                    hyper::StatusCode::INTERNAL_SERVER_ERROR,
                                    "Internal Server Error",
                                )
                            }
                        })
                    }
                }))
            }
        }),
    );

    server.await?;

    Ok(())
}
