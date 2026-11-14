//! What every page shares: the document (`<head>` with SEO, share card and PWA tags, the
//! browser bundle), the navbar, the footer and the small reusable blocks.

use active::html::svg;
use active::prelude::*;

use super::Site;
use super::pwa;
use crate::i18n::t;
use crate::icons::{Icon, icon};
use crate::routes::{Lang, Page, Route, doc_href, href};
use crate::{AUTHOR, GITHUB};

/// What a request needs to render a page.
pub struct Ctx<'a> {
    pub site: &'a Site,
    pub route: Route,
    /// Absolute origin for canonical and Open Graph URLs (`https://example.com`).
    pub origin: &'a str,
}

/// The license, served by the site itself: proprietary, all rights reserved.
pub const LICENSE_PATH: &str = "/static/LICENSE.txt";
/// The share card: 1200×630.
pub const OG_IMAGE: &str = "/static/og-image.png";

/// The sister projects of Nuage DB, by the same author: (name, website), in the order of the
/// brief; `i18n::Texts::footer_projects` describes Nuage DB then each one, in the same order.
pub const ECOSYSTEM: [(&str, &str); 7] = [
    ("Paquets", "https://paquets-90dfc06b6fea.herokuapp.com/"),
    ("Vitesse", "https://vitesse-f16135654c3d.herokuapp.com/"),
    ("Active", "https://active-c1883a966650.herokuapp.com/"),
    ("Memoire", "https://memoire-7f4525f1fdaf.herokuapp.com/"),
    ("Securite", "https://securite-2db4c1994be7.herokuapp.com/"),
    ("Facile", "https://github.com/maxlestage/Facile"),
    ("Rythme", "https://maxlestage.github.io/Rythme/"),
];

/// `<a target="_blank">` to another site.
pub fn external(url: impl Into<String>) -> Element {
    a().href(url.into()).attr("target", "_blank").attr("rel", "noopener noreferrer")
}

/// The Nuage DB mark: a cloud holding a database, on a sky-to-violet tile.
pub fn logo_mark(size: u32) -> Element {
    let gradient = |id: &'static str, stops: &[(&'static str, &'static str)]| {
        stops.iter().fold(
            linear_gradient().id(id).attr("x1", "0").attr("y1", "0").attr("x2", "1").attr("y2", "1"),
            |g, (offset, color)| g.child(stop().attr("offset", *offset).attr("stop-color", *color)),
        )
    };
    svg()
        .class("logo-mark")
        .attr("viewBox", "0 0 64 64")
        .attr("width", size.to_string())
        .attr("height", size.to_string())
        .attr("aria-hidden", "true")
        .child(defs().child(gradient("nlg", &[("0", "#38bdf8"), (".55", "#6366f1"), ("1", "#a855f7")])))
        .child(rect().attr("width", "64").attr("height", "64").attr("rx", "16").attr("fill", "url(#nlg)"))
        .child(
            path()
                .attr("d", "M19 46h27a10 10 0 0 0 1.6-19.9A14 14 0 0 0 20.6 23.3 11.5 11.5 0 0 0 19 46z")
                .attr("fill", "#fff"),
        )
        .child(
            g().attr("fill", "none")
                .attr("stroke", "#4f46e5")
                .attr("stroke-width", "2.6")
                .attr("stroke-linecap", "round")
                .child(ellipse().attr("cx", "33").attr("cy", "29.5").attr("rx", "7").attr("ry", "2.6"))
                .child(
                    path().attr(
                        "d",
                        "M26 29.5v9c0 1.4 3.1 2.6 7 2.6s7-1.2 7-2.6v-9M26 34c0 1.4 3.1 2.6 7 2.6s7-1.2 7-2.6",
                    ),
                ),
        )
}

/// The logo, linking to the home page.
pub fn logo(lang: Lang) -> Element {
    a().class("logo")
        .href(href(lang, Page::Home))
        .attr("aria-label", "Nuage DB")
        .child(logo_mark(32))
        .child(span().class("logo-text").text("Nuage").child(span().class("logo-accent").text(" DB")))
}

/// EN / FR / ES: links to the same page in each language, kept in the browser (no
/// cookie); Auto: the browser's language again.
pub fn lang_switch(route: Route) -> Element {
    let auto_label = match route.lang {
        Lang::Fr => "Comme l'appareil",
        Lang::Es => "Como el dispositivo",
        _ => "As the device",
    };
    // The same page without its language: the server picks the browser's.
    let bare = if route.page == Page::NotFound {
        "/".to_owned()
    } else {
        let path = route.path();
        let rest = path.strip_prefix(&format!("/{}", route.lang.code())).unwrap_or("/");
        if rest.is_empty() { "/".to_owned() } else { rest.to_owned() }
    };
    div()
        .class("lang-switch")
        .attr("role", "group")
        .attr("aria-label", t(route.lang).language)
        .children(Lang::ALL.map(|l| {
            let target = if route.page == Page::NotFound { Route::new(l, Page::Home) } else { route.with_lang(l) };
            let link = a()
                .class("lang-btn")
                .href(target.path())
                .attr("hreflang", l.code())
                .attr("lang", l.code())
                .attr("title", l.native_name())
                .text(l.short());
            if l == route.lang { link.class("active").attr("aria-current", "true") } else { link }
        }))
        .child(
            a().class("lang-btn lang-auto")
                .href(bare)
                .attr("title", auto_label)
                .attr("aria-label", auto_label)
                .text("Auto"),
        )
}

/// Light, dark, or as the device: stored in the browser (no cookie), applied before the
/// first paint by [`THEME_SCRIPT`].
pub fn theme_switch(lang: Lang) -> Element {
    let (group, light, dark, auto) = match lang {
        Lang::Fr => ("Thème", "Clair", "Sombre", "Comme l'appareil"),
        Lang::Es => ("Tema", "Claro", "Oscuro", "Como el dispositivo"),
        _ => ("Theme", "Light", "Dark", "As the device"),
    };
    let button = |code: &'static str, glyph: &'static str, label: &'static str| {
        el("button")
            .class("theme-btn")
            .attr("type", "button")
            .attr("data-set-theme", code)
            .attr("title", label)
            .attr("aria-label", label)
            .text(glyph)
    };
    div()
        .class("theme-switch")
        .attr("role", "group")
        .attr("aria-label", group)
        .child(button("light", "☀", light))
        .child(button("dark", "☾", dark))
        .child(button("auto", "A", auto))
}

/// Applies the theme stored in the browser before the page paints, and handles the
/// theme buttons (the Content-Security-Policy allows it by its hash).
pub const THEME_SCRIPT: &str = r#"(function(){var d=document.documentElement,k="nuage-theme",L="nuage-lang",s=null;try{var t=localStorage.getItem(k);if(t==="light"||t==="dark")d.setAttribute("data-theme",t);s=localStorage.getItem(L)}catch(e){}var q=location.search,m=/([?&])lang=auto(&|$)/;if(m.test(q)){var p=location.pathname,r=q.replace(m,function(_,a,b){return b?a:""}).replace(/[?&]$/,"");if(s&&s!==d.lang&&/^\/(en|fr|es)(\/|$)/.test(p)){location.replace(p.replace(/^\/(en|fr|es)/,"/"+s)+r+location.hash);return}history.replaceState(null,"",p+r+location.hash)}if(!s)d.setAttribute("data-lang-choice","auto");document.addEventListener("click",function(e){var c=e.target.closest;if(!c)return;var b=e.target.closest("[data-set-theme]");if(b){var t=b.getAttribute("data-set-theme");try{t==="auto"?localStorage.removeItem(k):localStorage.setItem(k,t)}catch(x){}t==="auto"?d.removeAttribute("data-theme"):d.setAttribute("data-theme",t);return}var a=e.target.closest(".lang-btn");if(a){try{var g=a.getAttribute("hreflang");g?localStorage.setItem(L,g):localStorage.removeItem(L)}catch(x){}}})})();"#;

/// Eyebrow, title and subtitle of a section.
pub fn section_head(eyebrow: &str, title: &str, subtitle: Option<&str>, id: &str) -> Element {
    div()
        .class("section-head")
        .child(span().class("eyebrow").text(eyebrow.to_string()))
        .child(h2().id(id.to_string()).class("section-title").text(title.to_string()))
        .child(subtitle.map(|s| p().class("section-sub").text(s.to_string())))
}

/// The heading block of the inner pages.
pub fn page_hero(eyebrow: &str, title: &str, subtitle: &str) -> Element {
    section()
        .class("page-hero container")
        .child(span().class("eyebrow").text(eyebrow.to_string()))
        .child(h1().class("page-title").text(title.to_string()))
        .child(p().class("page-sub").text(subtitle.to_string()))
}

/// A code window: a label and already highlighted code.
pub fn code_window(label: &str, code_html: String) -> Element {
    figure()
        .class("code-window")
        .child(figcaption().class("code-head").child(dots()).child(span().class("code-label").text(label.to_string())))
        .child(pre().child(code().html(code_html)))
}

fn dots() -> Element {
    span().class("dots").attr("aria-hidden", "true").child(i()).child(i()).child(i())
}

fn navbar(route: Route) -> Element {
    let lang = route.lang;
    let tx = t(lang);
    let links = [
        (Page::Features, tx.nav_features),
        (Page::Docs, tx.nav_docs),
        (Page::Faq, tx.nav_faq),
        (Page::App, tx.nav_status),
    ];
    let link = move |page: Page, label: &'static str, class: &'static str| {
        let a = a().class(class).href(href(lang, page));
        let a = if page == Page::App { a.child(span().class("live-dot").attr("aria-hidden", "true")) } else { a };
        let a = a.text(label);
        if route.page == page { a.class("active").attr("aria-current", "page") } else { a }
    };
    header().class("navbar").child(
        div()
            .class("nav-inner container")
            .child(logo(lang))
            .child(
                nav()
                    .class("nav-links")
                    .attr("aria-label", tx.nav_main)
                    .children(links.map(|(page, label)| link(page, label, "nav-link"))),
            )
            .child(
                div()
                    .class("nav-actions")
                    .child(theme_switch(lang))
                    .child(lang_switch(route))
                    .child(a().class("btn btn-ghost btn-sm").href("/admin").text(tx.nav_console))
                    .child(
                        a().class("btn btn-primary btn-sm nav-cta")
                            .href(doc_href(lang, "quick-start"))
                            .text(tx.cta_start),
                    ),
            )
            .child(
                details()
                    .class("menu")
                    .child(
                        el("summary")
                            .class("menu-button")
                            .attr("aria-label", tx.nav_menu)
                            .child(icon(Icon::Menu))
                            .child(span().class("sr-only").text(tx.nav_menu)),
                    )
                    .child(
                        div()
                            .class("menu-panel")
                            .child(
                                nav()
                                    .class("menu-links")
                                    .attr("aria-label", tx.nav_menu)
                                    .child(link(Page::Home, tx.nav_home, "menu-link"))
                                    .children(links.map(|(page, label)| link(page, label, "menu-link")))
                                    .child(a().class("menu-link").href("/admin").text(tx.nav_console)),
                            )
                            .child(lang_switch(route))
                            .child(theme_switch(lang))
                            .child(
                                a().class("btn btn-primary")
                                    .href(doc_href(lang, "quick-start"))
                                    .text(tx.cta_start)
                                    .child(icon(Icon::ArrowRight)),
                            ),
                    ),
            ),
    )
}

/// The professional footer, shared in spirit with every site of the ecosystem: the call to
/// action, the link columns, the sister projects, the author's signature, the wordmark and
/// the legal line.
fn page_footer(route: Route) -> Element {
    let lang = route.lang;
    let tx = t(lang);
    let item = |url: String, label: &'static str| li().child(a().href(url).text(label));
    let ext = |url: &'static str, label: &'static str| {
        li().child(external(url).text(label).child(span().class("ext").child(icon(Icon::External))))
    };
    let column = |title: &'static str, links: Vec<Element>| {
        div().class("footer-col").child(h2().class("footer-h").text(title)).child(ul().children(links))
    };
    let cta = div()
        .class("container footer-cta")
        .child(
            a().class("footer-cta-link")
                .href(doc_href(lang, "quick-start"))
                .child(span().text(tx.footer_start))
                .child(Node::from(" "))
                .child(em().text("Nuage DB"))
                .child(icon(Icon::ArrowRight)),
        )
        .child(el("code").class("footer-install").text("let db = NuageDb::open(\"data/\")?;"));
    let brand = div()
        .class("footer-brand")
        .child(logo(lang))
        .child(p().class("footer-tagline").text(tx.footer_tagline))
        .child(p().class("footer-mission").text(tx.footer_mission))
        .child(
            ul().class("footer-badges")
                .attr("aria-label", "Nuage DB")
                .child(li().class("hot").text(format!("v{}", env!("CARGO_PKG_VERSION"))))
                .child(li().text("100 % Rust"))
                .child(li().text(tx.footer_no_cookies)),
        )
        .child(
            div()
                .class("footer-social")
                .child(external(GITHUB).class("icon-btn").attr("aria-label", "GitHub").child(icon(Icon::Github))),
        );
    let product = column(
        tx.footer_product,
        vec![
            item(href(lang, Page::Features), tx.nav_features),
            item(href(lang, Page::Docs), tx.nav_docs),
            item(href(lang, Page::App), tx.nav_status),
            item("/admin".into(), tx.nav_console),
            item(href(lang, Page::Changelog), tx.footer_changelog),
            ext(GITHUB, "GitHub"),
        ],
    );
    let resources = column(
        tx.footer_resources,
        vec![
            item(doc_href(lang, "quick-start"), tx.footer_quick_start),
            item(doc_href(lang, "deploy"), tx.footer_deploy),
            item(doc_href(lang, "heroku"), "Heroku"),
            item(doc_href(lang, "status"), "API"),
            item(href(lang, Page::Faq), tx.nav_faq),
        ],
    );
    let legal = column(
        tx.footer_legal,
        vec![
            item(format!("{}#license", href(lang, Page::Legal)), tx.footer_license),
            item(format!("{}#privacy", href(lang, Page::Legal)), tx.footer_privacy),
            li().class("footer-note").child(icon(Icon::Shield)).text(tx.footer_no_cookies),
        ],
    );
    let status = div()
        .class("footer-col footer-status")
        .child(h2().class("footer-h").text(tx.footer_status))
        .child(
            a().class("status-link")
                .href(href(lang, Page::App))
                .child(span().class("live-dot").attr("aria-hidden", "true"))
                .text(tx.footer_status_text)
                .child(icon(Icon::ArrowRight)),
        )
        .child(h2().class("footer-h with-icon").child(icon(Icon::Globe)).text(tx.language))
        .child(lang_switch(route));
    let projects = std::iter::once(("Nuage DB", "")).chain(ECOSYSTEM);
    let ecosystem = nav()
        .class("container footer-eco")
        .attr("aria-label", tx.footer_ecosystem)
        .child(
            div()
                .class("footer-eco-head")
                .child(h2().class("footer-h").text(tx.footer_ecosystem))
                .child(p().text(tx.footer_ecosystem_text)),
        )
        .child(ul().class("eco-grid").children(projects.zip(tx.footer_projects).map(|((name, url), text)| {
            let here = url.is_empty();
            let mut title = span().class("eco-name").text(name);
            if here {
                title = title.child(span().class("eco-here").text(tx.footer_here));
            }
            let link = if here { a().href(href(lang, Page::Home)).attr("aria-current", "page") } else { external(url) };
            li().child(
                link.class(if here { "eco current" } else { "eco" })
                    .child(title)
                    .child(span().class("eco-text").text(text))
                    .child(icon(Icon::ArrowRight)),
            )
        })));
    let credits = div()
        .class("container footer-credits")
        .child(p().class("credits-by").text(tx.footer_by))
        .child(p().class("credits-name").text(AUTHOR))
        .child(p().class("credits-note").text(tx.footer_crafted));
    let wordmark = div().class("wordmark").attr("aria-hidden", "true").text("Nuage");
    footer()
        .class("footer")
        .child(cta)
        .child(
            div()
                .class("container footer-grid")
                .child(brand)
                .child(product)
                .child(resources)
                .child(legal)
                .child(status),
        )
        .child(ecosystem)
        .child(credits)
        .child(wordmark)
        .child(
            div()
                .class("container footer-bottom")
                .child(p().text(tx.footer_rights))
                .child(a().href(LICENSE_PATH).text(tx.footer_proprietary))
                .child(span().class("footer-built").text(tx.footer_built))
                .child(a().class("footer-top").href("#top").text(tx.back_to_top).child(icon(Icon::ArrowUp))),
        )
}

/// "A new version is available": hidden until the browser bundle finds a waiting service
/// worker.
fn update_toast(lang: Lang) -> Element {
    let tx = t(lang);
    div()
        .id("update-toast")
        .class("toast")
        .attr("role", "status")
        .attr("hidden", "")
        .child(icon(Icon::Refresh))
        .child(span().text(tx.update_available))
        .child(
            button().attr("type", "button").id("update-reload").class("btn btn-primary btn-sm").text(tx.update_reload),
        )
}

/// A JSON string literal (for JSON-LD).
pub fn json(s: &str) -> String {
    serde_json::to_string(s).expect("a string")
}

/// The share card and search tags of a page.
fn seo(ctx: &Ctx<'_>, title: &str, description: &str) -> Seo {
    let route = ctx.route;
    let tx = t(route.lang);
    let mut seo = Seo::new()
        .site_name("Nuage DB")
        .title(title.to_string())
        .title_template("%s · Nuage DB")
        .description(description.to_string())
        .base_url(ctx.origin)
        .canonical(route.path())
        .image(OG_IMAGE)
        .locale(route.lang.locale())
        .meta_property("og:image:width", "1200")
        .meta_property("og:image:height", "630")
        .meta_property("og:image:type", "image/png")
        .meta_property("og:image:alt", tx.og_alt)
        .meta_name("twitter:title", format!("{title} · Nuage DB"))
        .meta_name("twitter:description", description.to_string())
        .meta_name("twitter:image", format!("{}{OG_IMAGE}", ctx.origin))
        .meta_name("twitter:image:alt", tx.og_alt);
    if route.page != Page::NotFound {
        for l in Lang::ALL {
            seo = seo.alternate(l.code(), route.with_lang(l).path());
        }
        seo = seo.alternate("x-default", route.with_lang(Lang::En).path());
    } else {
        seo = seo.noindex(true);
    }
    seo
}

/// The `<head>` and `<html>` every document shares: stylesheet, icons, PWA tags, the
/// service worker and the browser bundle.
pub fn base_document(site: &Site, body: Node, lang: Lang) -> Document {
    let assets = &site.assets;
    let mut doc = Document::new(body)
        .lang(lang.code())
        .viewport("width=device-width, initial-scale=1, viewport-fit=cover")
        .stylesheet(assets.url("/static/app.css"))
        .head(format!(
            concat!(
                r#"<meta name="author" content="{author}"/>"#,
                r#"<link rel="preload" href="/static/fonts/inter-latin.woff2" as="font" type="font/woff2" crossorigin=""/>"#,
            ),
            author = AUTHOR
        ))
        .head(format!("<script>{THEME_SCRIPT}</script>"))
        .head(pwa::head_tags(lang))
        .service_worker("/sw.js");
    if assets.has_bundle() {
        doc = doc.client(assets.url("/pkg/nuage_site.js"), assets.url("/pkg/nuage_site_bg.wasm"));
    }
    doc
}

/// A complete page around `content`.
pub fn document(ctx: &Ctx<'_>, title: &str, description: &str, content: Node, json_ld: Option<String>) -> String {
    let route = ctx.route;
    let lang = route.lang;
    let tx = t(lang);
    let body = fragment([
        Node::from(a().class("skip-link").href("#main").text(tx.skip_to_content)),
        div().class("bg").attr("aria-hidden", "true").into(),
        navbar(route).into(),
        main().id("main").class("main").attr("tabindex", "-1").child(content).into(),
        page_footer(route).into(),
        update_toast(lang).into(),
    ]);
    let mut seo = seo(ctx, title, description);
    if let Some(json_ld) = json_ld {
        seo = seo.json_ld(json_ld);
    }
    let other_locales: String = Lang::ALL
        .into_iter()
        .filter(|l| *l != lang)
        .map(|l| format!(r#"<meta property="og:locale:alternate" content="{}"/>"#, l.locale()))
        .collect();
    base_document(ctx.site, body, lang).body_attr("id", "top").seo(seo).head(other_locales).render()
}
