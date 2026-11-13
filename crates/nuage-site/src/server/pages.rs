//! The pages: home, features, documentation, FAQ, changelog, legal, the live dashboard,
//! the 404 and the offline page.

use active::prelude::*;

use super::highlight::highlight;
use super::layout::{self, Ctx, LICENSE_PATH, code_window, external, page_hero, section_head};
use crate::i18n::{Texts, t};
use crate::icons::{FEATURE_ICONS, Icon, icon};
use crate::routes::{Lang, Page, doc_href, href};
use crate::{AUTHOR, GITHUB};

/// The code samples (the same in every language).
pub mod code {
    pub const EMBEDDED: &str = r#"use memoire_orm::prelude::*;

// Embedded: the database lives in your process, in data/.
let db = NuageDb::open("data/")?;

db.create_table_if_not_exists::<User>().await?;
let adults = User::query()
    .filter(User::AGE.ge(18))
    .all(&db)
    .await?;"#;

    pub const CONNECT: &str = r#"// Server mode: the same Memoire API, over HTTP.
let db = NuageDb::connect("https://nuage.example.com", token)?;

let adults = User::query()
    .filter(User::AGE.ge(18))
    .all(&db)
    .await?;"#;

    pub const HERO: &str = r#"// Embedded, in your process…
let db = NuageDb::open("data/")?;
// …or a server, over HTTP: the same API.
let db = NuageDb::connect(url, token)?;

let users = User::query().all(&db).await?;"#;

    pub const SERVER: &str = r#"# Database, /admin, /app and the status API, on one port.
# Everything it stores is encrypted with NUAGE_ENCRYPTION_KEY: keep it safe.
NUAGE_ENCRYPTION_KEY=$(nuage genkey) NUAGE_TOKEN=change-me PORT=8080 nuage
# More applications, each with its own encrypted database and tokens: /admin."#;

    pub const STATUS: &str = r#"curl -H "Authorization: Bearer $NUAGE_TOKEN" https://nuage.example.com/api/status
curl -N -H "Authorization: Bearer $NUAGE_TOKEN" https://nuage.example.com/api/status/stream
curl https://nuage.example.com/api/health"#;

    pub const PUBLIC: &str = r#"# Show the public subset of /api/status and /app without a token.
NUAGE_PUBLIC_STATUS=true"#;

    pub const HEROKU: &str = r#"# Procfile
web: nuage

heroku config:set NUAGE_ENCRYPTION_KEY=$(openssl rand -hex 32) NUAGE_TOKEN=change-me"#;
}

/// Several nodes side by side.
fn nodes(list: Vec<Node>) -> Node {
    fragment(list)
}

fn rust(label: &str, src: &str) -> Element {
    code_window(label, highlight(src, "rust"))
}

fn shell(label: &str, src: &str) -> Element {
    code_window(label, highlight(src, "sh"))
}

fn faq_list(items: &[(&'static str, &'static str)]) -> Element {
    div().class("faq").children(
        items.iter().map(|(q, a)| details().class("faq-item card").child(el("summary").text(*q)).child(p().text(*a))),
    )
}

fn home(ctx: &Ctx<'_>) -> String {
    let lang = ctx.route.lang;
    let tx = t(lang);
    let hero = section()
        .class("hero container")
        .attr("aria-labelledby", "hero-title")
        .child(
            div()
                .class("hero-copy")
                .child(span().class("eyebrow").text(tx.hero_eyebrow))
                .child(h1().id("hero-title").class("hero-title").text(tx.hero_title))
                .child(p().class("hero-lead").text(tx.hero_lead))
                .child(
                    div()
                        .class("hero-ctas")
                        .child(
                            a().class("btn btn-primary btn-lg")
                                .href(doc_href(lang, "quick-start"))
                                .text(tx.cta_start)
                                .child(icon(Icon::ArrowRight)),
                        )
                        .child(
                            a().class("btn btn-ghost btn-lg")
                                .href(href(lang, Page::App))
                                .child(span().class("live-dot").attr("aria-hidden", "true"))
                                .text(tx.cta_status),
                        ),
                )
                .child(
                    ul().class("hero-points").children(tx.hero_points.map(|p| li().child(icon(Icon::Check)).text(p))),
                ),
        )
        .child(
            div()
                .class("hero-visual")
                .child(div().class("cloud-glow").attr("aria-hidden", "true"))
                .child(rust("main.rs", code::HERO)),
        );

    let what = section()
        .class("section container")
        .attr("aria-labelledby", "what")
        .child(section_head(tx.what_eyebrow, tx.what_title, Some(tx.what_text), "what"))
        .child(ul().class("grid-3").children(tx.what_points.iter().zip([Icon::Code, Icon::Gear, Icon::Cloud]).map(
            |((title, text), i)| {
                li().class("card point")
                    .child(span().class("point-icon").child(icon(i)))
                    .child(h3().text(*title))
                    .child(p().text(*text))
            },
        )));

    let features = section()
        .class("section container")
        .attr("aria-labelledby", "features")
        .child(section_head(tx.features_eyebrow, tx.features_title, Some(tx.features_lead), "features"))
        .child(ul().class("feature-grid").children(tx.features.iter().zip(FEATURE_ICONS).map(|(f, i)| {
            li().class("card feature")
                .child(span().class("point-icon").child(icon(i)))
                .child(h3().text(f.title))
                .child(p().text(f.desc))
        })))
        .child(
            p().class("section-more").child(
                a().class("btn btn-ghost")
                    .href(href(lang, Page::Features))
                    .text(tx.features_more)
                    .child(icon(Icon::ArrowRight)),
            ),
        );

    let modes = section()
        .class("section container")
        .attr("aria-labelledby", "modes")
        .child(section_head(tx.modes_eyebrow, tx.modes_title, Some(tx.modes_lead), "modes"))
        .child(
            div()
                .class("grid-2")
                .child(
                    div()
                        .class("card mode")
                        .child(h3().child(icon(Icon::Layers)).text(tx.modes[0].0))
                        .child(p().text(tx.modes[0].1))
                        .child(rust("embedded.rs", code::EMBEDDED)),
                )
                .child(
                    div()
                        .class("card mode")
                        .child(h3().child(icon(Icon::Server)).text(tx.modes[1].0))
                        .child(p().text(tx.modes[1].1))
                        .child(rust("server.rs", code::CONNECT)),
                ),
        );

    let quick = section()
        .class("section container")
        .attr("aria-labelledby", "quick-start")
        .child(section_head(tx.quick_eyebrow, tx.quick_title, Some(tx.quick_lead), "quick-start"))
        .child(
            div()
                .class("grid-2 quick")
                .child(ol().class("steps").children(tx.quick_steps.iter().enumerate().map(|(i, (title, text))| {
                    li().class("step")
                        .child(span().class("step-n").attr("aria-hidden", "true").text(format!("{:02}", i + 1)))
                        .child(div().child(h3().text(*title)).child(p().text(*text)))
                })))
                .child(
                    div().class("stack").child(shell("terminal", code::SERVER)).child(rust("main.rs", code::CONNECT)),
                ),
        );

    let deploy = section()
        .class("section container")
        .attr("aria-labelledby", "deploy")
        .child(section_head(tx.deploy_eyebrow, tx.deploy_title, Some(tx.deploy_lead), "deploy"))
        .child(ul().class("grid-3").children(
            tx.deploy_points.iter().zip([Icon::Server, Icon::Cloud, Icon::Activity]).map(|((title, text), i)| {
                li().class("card point")
                    .child(span().class("point-icon").child(icon(i)))
                    .child(h3().text(*title))
                    .child(p().text(*text))
            }),
        ))
        .child(
            p().class("section-more").child(
                a().class("btn btn-ghost")
                    .href(doc_href(lang, "heroku"))
                    .text(tx.docs[8].title)
                    .child(icon(Icon::ArrowRight)),
            ),
        );

    let fit = section()
        .class("section container")
        .attr("aria-labelledby", "ecosystem")
        .child(section_head(tx.fit_eyebrow, tx.fit_title, Some(tx.fit_lead), "ecosystem"))
        .child(ul().class("grid-3").children(
            tx.fit_items.iter().zip([Icon::Database, Icon::Layers, Icon::Globe]).map(|((title, text), i)| {
                li().class("card point")
                    .child(span().class("point-icon").child(icon(i)))
                    .child(h3().text(*title))
                    .child(p().text(*text))
            }),
        ));

    let faq = section()
        .class("section container")
        .attr("aria-labelledby", "faq")
        .child(section_head(tx.faq_eyebrow, tx.faq_title, Some(tx.faq_lead), "faq"))
        .child(faq_list(&tx.faq[..5]))
        .child(p().class("section-more").child(
            a().class("btn btn-ghost").href(href(lang, Page::Faq)).text(tx.faq_all).child(icon(Icon::ArrowRight)),
        ));

    let content = nodes(vec![
        hero.into(),
        what.into(),
        features.into(),
        modes.into(),
        quick.into(),
        deploy.into(),
        fit.into(),
        faq.into(),
        final_cta(lang).into(),
    ]);
    let json_ld = format!(
        concat!(
            r#"{{"@context":"https://schema.org","@type":"SoftwareApplication","name":"Nuage DB","#,
            r#""applicationCategory":"DeveloperApplication","operatingSystem":"Linux, macOS, Windows","#,
            r#""description":{},"url":{},"image":{},"codeRepository":{},"inLanguage":{},"#,
            r#""author":{{"@type":"Person","name":{}}}}}"#
        ),
        layout::json(tx.meta_description),
        layout::json(&format!("{}{}", ctx.origin, ctx.route.path())),
        layout::json(&format!("{}{}", ctx.origin, layout::OG_IMAGE)),
        layout::json(GITHUB),
        layout::json(lang.code()),
        layout::json(AUTHOR),
    );
    layout::document(ctx, tx.tagline, tx.meta_description, content, Some(json_ld))
}

fn final_cta(lang: Lang) -> Element {
    let tx = t(lang);
    section().class("section container").child(
        div().class("final card").child(h2().text(tx.final_title)).child(p().text(tx.final_text)).child(
            div()
                .class("hero-ctas")
                .child(
                    a().class("btn btn-primary btn-lg")
                        .href(doc_href(lang, "quick-start"))
                        .text(tx.cta_start)
                        .child(icon(Icon::ArrowRight)),
                )
                .child(external(GITHUB).class("btn btn-ghost btn-lg").child(icon(Icon::Github)).text("GitHub")),
        ),
    )
}

fn features(ctx: &Ctx<'_>) -> String {
    let lang = ctx.route.lang;
    let tx = t(lang);
    let content = nodes(vec![
        page_hero(tx.features_eyebrow, tx.features_title, tx.features_page_sub).into(),
        section()
            .class("container section-tight")
            .child(ul().class("feature-list").children(tx.features.iter().zip(FEATURE_ICONS).map(|(f, i)| {
                li().class("card feature-detail").child(span().class("point-icon").child(icon(i))).child(
                    div().child(h2().text(f.title)).child(p().class("lead").text(f.desc)).child(p().text(f.detail)),
                )
            })))
            .into(),
        section()
            .class("container section-tight")
            .attr("aria-labelledby", "admin")
            .child(
                div()
                    .class("card admin")
                    .child(h2().id("admin").child(icon(Icon::Gear)).text(tx.admin_title))
                    .child(p().text(tx.admin_text))
                    .child(
                        ul().class("checks").children(tx.admin_points.map(|p| li().child(icon(Icon::Check)).text(p))),
                    )
                    .child(
                        div()
                            .class("hero-ctas")
                            .child(
                                a().class("btn btn-primary")
                                    .href("/admin")
                                    .text(tx.nav_console)
                                    .child(icon(Icon::ArrowRight)),
                            )
                            .child(
                                a().class("btn btn-ghost")
                                    .href(href(lang, Page::App))
                                    .child(span().class("live-dot").attr("aria-hidden", "true"))
                                    .text(tx.nav_status),
                            ),
                    ),
            )
            .into(),
        final_cta(lang).into(),
    ]);
    layout::document(ctx, tx.nav_features, tx.features_page_sub, content, None)
}

fn doc_code(id: &str) -> Vec<Element> {
    match id {
        "quick-start" => vec![rust("main.rs", code::EMBEDDED)],
        "server" => vec![shell("terminal", code::SERVER)],
        "connect" => vec![rust("main.rs", code::CONNECT)],
        "status" => vec![shell("terminal", code::STATUS), shell("env", code::PUBLIC)],
        "heroku" => vec![shell("Procfile", code::HEROKU)],
        _ => Vec::new(),
    }
}

fn docs(ctx: &Ctx<'_>) -> String {
    let lang = ctx.route.lang;
    let tx = t(lang);
    let toc = nav()
        .class("toc card")
        .attr("aria-label", tx.docs_toc)
        .child(p().class("toc-title").text(tx.docs_toc))
        .child(ol().children(tx.docs.iter().map(|d| li().child(a().href(format!("#{}", d.id)).text(d.title)))));
    let sections = div().class("doc-sections").children(tx.docs.iter().map(|d| {
        section()
            .class("doc-section")
            .attr("aria-labelledby", d.id)
            .child(h2().id(d.id).text(d.title))
            .child(p().text(d.text))
            .children(doc_code(d.id))
    }));
    let content = nodes(vec![
        page_hero(tx.nav_docs, tx.docs_title, tx.docs_sub).into(),
        div().class("container docs").child(toc).child(sections).into(),
    ]);
    layout::document(ctx, tx.docs_title, tx.docs_sub, content, None)
}

fn faq(ctx: &Ctx<'_>) -> String {
    let lang = ctx.route.lang;
    let tx = t(lang);
    let json_ld = format!(
        r#"{{"@context":"https://schema.org","@type":"FAQPage","mainEntity":[{}]}}"#,
        tx.faq
            .iter()
            .map(|(q, a)| format!(
                r#"{{"@type":"Question","name":{},"acceptedAnswer":{{"@type":"Answer","text":{}}}}}"#,
                layout::json(q),
                layout::json(a)
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let content = nodes(vec![
        page_hero(tx.faq_eyebrow, tx.faq_title, tx.faq_page_sub).into(),
        section().class("container section-tight").child(faq_list(&tx.faq)).into(),
        final_cta(lang).into(),
    ]);
    layout::document(ctx, tx.nav_faq, tx.faq_page_sub, content, Some(json_ld))
}

fn changelog(ctx: &Ctx<'_>) -> String {
    let tx = t(ctx.route.lang);
    let content = nodes(vec![
        page_hero(tx.footer_changelog, tx.changelog_title, tx.changelog_sub).into(),
        section()
            .class("container section-tight")
            .child(article().class("card release").child(h2().text(tx.changelog_first)).child(
                ul().class("checks").children(tx.changelog_items.map(|i| li().child(icon(Icon::Check)).text(i))),
            ))
            .into(),
    ]);
    layout::document(ctx, tx.changelog_title, tx.changelog_sub, content, None)
}

fn legal(ctx: &Ctx<'_>) -> String {
    let tx = t(ctx.route.lang);
    let content = nodes(vec![
        page_hero(tx.footer_legal, tx.legal_title, tx.legal_sub).into(),
        section()
            .class("container section-tight legal")
            .child(
                article()
                    .class("card")
                    .attr("aria-labelledby", "license")
                    .child(h2().id("license").text(tx.license_title))
                    .child(p().text(tx.license_text))
                    .child(p().child(a().href(LICENSE_PATH).text(tx.license_read).child(icon(Icon::ArrowRight)))),
            )
            .child(
                article()
                    .class("card")
                    .attr("aria-labelledby", "privacy")
                    .child(h2().id("privacy").text(tx.privacy_title))
                    .children(tx.privacy_text.map(|text| p().text(text))),
            )
            .into(),
    ]);
    layout::document(ctx, tx.legal_title, tx.legal_sub, content, None)
}

fn app(ctx: &Ctx<'_>) -> String {
    let lang = ctx.route.lang;
    let tx = t(lang);
    let content = nodes(vec![
        section()
            .class("container app-head")
            .child(
                h1().class("app-title").child(span().class("live-dot").attr("aria-hidden", "true")).text(tx.app_title),
            )
            .child(p().class("page-sub").text(tx.app_sub))
            .into(),
        section()
            .class("container app")
            .attr("aria-label", tx.app_title)
            .child(island("dashboard", lang.code(), crate::dashboard::dashboard))
            .into(),
    ]);
    layout::document(ctx, tx.app_title, tx.app_sub, content, None)
}

fn not_found(ctx: &Ctx<'_>) -> String {
    let lang = ctx.route.lang;
    let tx = t(lang);
    let content = section()
        .class("container not-found")
        .child(p().class("code-404").attr("aria-hidden", "true").text("404"))
        .child(h1().class("page-title").text(tx.not_found_title))
        .child(p().class("page-sub").text(tx.not_found_text))
        .child(
            a().class("btn btn-primary").href(href(lang, Page::Home)).text(tx.back_home).child(icon(Icon::ArrowRight)),
        );
    layout::document(ctx, tx.not_found_title, tx.not_found_text, content.into(), None)
}

/// The page of a route.
pub fn render(ctx: &Ctx<'_>) -> String {
    match ctx.route.page {
        Page::Home => home(ctx),
        Page::Features => features(ctx),
        Page::Docs => docs(ctx),
        Page::Faq => faq(ctx),
        Page::Changelog => changelog(ctx),
        Page::Legal => legal(ctx),
        Page::App => app(ctx),
        Page::NotFound => not_found(ctx),
    }
}

/// `/offline/`: the page the service worker shows for a page never opened, offline. It
/// speaks every language (it is the same for everyone).
pub fn offline(site: &super::Site) -> String {
    let block = |tx: &Texts, lang: Lang| {
        div()
            .class("offline-block")
            .attr("lang", lang.code())
            .child(h2().text(tx.offline_title))
            .child(p().text(tx.offline_text))
            .child(
                p().class("hero-ctas")
                    .child(a().class("btn btn-primary").href(href(lang, Page::Home)).text(tx.back_home))
                    .child(a().class("btn btn-ghost").href(href(lang, Page::App)).text(tx.nav_status)),
            )
    };
    let body = main()
        .id("main")
        .class("main offline container")
        .child(layout::logo(Lang::En))
        .child(h1().class("page-title").child(icon(Icon::WifiOff)).text(t(Lang::En).offline_title))
        .children(Lang::ALL.map(|l| block(t(l), l)))
        .child(p().child(a().class("btn btn-ghost").href("").text(t(Lang::En).offline_retry)));
    let seo = Seo::new().title("Offline · Nuage DB").noindex(true);
    layout::base_document(site, Node::from(body), Lang::En).seo(seo).render()
}
