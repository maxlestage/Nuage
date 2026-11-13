//! URLs of the site: `/{lang}/`, `/{lang}/features/`, `/{lang}/docs/`, `/{lang}/faq/`,
//! `/{lang}/changelog/`, `/{lang}/legal/` and the live dashboard `/{lang}/app/`.
//! `/app` (what the Nuage DB server announces) leads to the dashboard in the visitor's
//! language.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    En,
    Fr,
    Es,
}

impl Lang {
    /// The languages of the site, the same as the Memoire website.
    pub const ALL: [Lang; 3] = [Lang::En, Lang::Fr, Lang::Es];

    pub fn idx(self) -> usize {
        match self {
            Lang::En => 0,
            Lang::Fr => 1,
            Lang::Es => 2,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Fr => "fr",
            Lang::Es => "es",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Lang::En => "EN",
            Lang::Fr => "FR",
            Lang::Es => "ES",
        }
    }

    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Fr => "Français",
            Lang::Es => "Español",
        }
    }

    /// `og:locale`.
    pub fn locale(self) -> &'static str {
        match self {
            Lang::En => "en_US",
            Lang::Fr => "fr_FR",
            Lang::Es => "es_ES",
        }
    }

    /// `"fr"`, `"fr-FR"`, `"FR_ca"`… → the language, if the site has it.
    pub fn parse(code: &str) -> Option<Lang> {
        let code = code.trim().to_ascii_lowercase();
        match code.split(['-', '_']).next().unwrap_or_default() {
            "en" => Some(Lang::En),
            "fr" => Some(Lang::Fr),
            "es" => Some(Lang::Es),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    Home,
    Features,
    Docs,
    Faq,
    Changelog,
    Legal,
    /// The live status dashboard.
    App,
    NotFound,
}

impl Page {
    /// Every page that exists (not the 404).
    pub const ALL: [Page; 7] =
        [Page::Home, Page::Features, Page::Docs, Page::Faq, Page::Changelog, Page::Legal, Page::App];

    /// The page of the path segments after the language (`["docs"]`).
    pub fn parse(parts: &[&str]) -> Page {
        match parts {
            [] => Page::Home,
            ["features"] => Page::Features,
            ["docs"] => Page::Docs,
            ["faq"] => Page::Faq,
            ["changelog"] => Page::Changelog,
            ["legal"] => Page::Legal,
            ["app"] => Page::App,
            _ => Page::NotFound,
        }
    }

    /// The path after the language, with slashes on both ends (`/docs/`).
    pub fn path(self) -> &'static str {
        match self {
            Page::Home => "/",
            Page::Features => "/features/",
            Page::Docs => "/docs/",
            Page::Faq => "/faq/",
            Page::Changelog => "/changelog/",
            Page::Legal => "/legal/",
            Page::App => "/app/",
            Page::NotFound => "/404/",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub lang: Lang,
    pub page: Page,
}

impl Route {
    pub fn new(lang: Lang, page: Page) -> Route {
        Route { lang, page }
    }

    /// The route of `path`, `None` when it does not start with a language.
    pub fn parse(path: &str) -> Option<Route> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let first = parts.first()?;
        let lang = Lang::ALL.into_iter().find(|l| l.code() == *first)?;
        Some(Route { lang, page: Page::parse(&parts[1..]) })
    }

    /// `/fr/docs/`.
    pub fn path(&self) -> String {
        format!("/{}{}", self.lang.code(), self.page.path())
    }

    /// The same page in another language.
    pub fn with_lang(&self, lang: Lang) -> Route {
        Route { lang, page: self.page }
    }
}

/// A link to `page` in `lang`.
pub fn href(lang: Lang, page: Page) -> String {
    Route::new(lang, page).path()
}

/// A link to a section of the documentation (`/fr/docs/#deploy`).
pub fn doc_href(lang: Lang, anchor: &str) -> String {
    format!("{}#{anchor}", href(lang, Page::Docs))
}

/// The pages that the site answers without a language, redirected to the visitor's
/// language: `/app` (the dashboard), `/docs`, `/features`…
pub fn bare(path: &str) -> Option<Page> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    match Page::parse(&parts) {
        Page::NotFound => None,
        page => Some(page),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_builds_urls() {
        let app = Route::parse("/fr/app/").unwrap();
        assert_eq!(app, Route::new(Lang::Fr, Page::App));
        assert_eq!(app.path(), "/fr/app/");
        assert_eq!(app.with_lang(Lang::Es).path(), "/es/app/");
        assert_eq!(Route::parse("/en/").unwrap().page, Page::Home);
        assert_eq!(Route::parse("/en/docs").unwrap().page, Page::Docs);
        assert_eq!(Route::parse("/en/nope/").unwrap().page, Page::NotFound);
        assert_eq!(Route::parse("/de/"), None);
        assert_eq!(bare("/app"), Some(Page::App));
        assert_eq!(bare("/"), Some(Page::Home));
        assert_eq!(bare("/nope"), None);
        assert_eq!(doc_href(Lang::Fr, "deploy"), "/fr/docs/#deploy");
    }

    #[test]
    fn languages() {
        assert_eq!(Lang::parse("fr-CA"), Some(Lang::Fr));
        assert_eq!(Lang::parse("ES"), Some(Lang::Es));
        assert_eq!(Lang::parse("de"), None);
    }
}
