//! Translations. Source strings are English; catalogs are standard gettext
//! .po files from po/, embedded at build time (see build.rs). By default the
//! language follows the desktop locale (LANGUAGE, LC_ALL, LC_MESSAGES, LANG);
//! `set_language` overrides it at runtime.

use std::collections::HashMap;
use std::fmt::{Display, Write};
use std::sync::{Mutex, RwLock};

use gtk::glib;

include!(concat!(env!("OUT_DIR"), "/catalogs.rs"));

type Catalog = HashMap<&'static str, &'static str>;

/// Languages without their own catalog that read another one comfortably.
const FALLBACKS: &[(&str, &str)] = &[("no", "nb"), ("nn", "nb")];

/// Native names for the language picker; unknown codes are shown as is.
const NATIVE_NAMES: &[(&str, &str)] = &[
    ("da", "Dansk"),
    ("de", "Deutsch"),
    ("en", "English"),
    ("es", "Español"),
    ("fi", "Suomi"),
    ("fr", "Français"),
    ("it", "Italiano"),
    ("nb", "Norsk bokmål"),
    ("nl", "Nederlands"),
    ("nn", "Norsk nynorsk"),
    ("pl", "Polski"),
    ("pt", "Português"),
    ("sv", "Svenska"),
];

/// The active catalog; `None` until first use, then the desktop language.
static ACTIVE: RwLock<Option<&'static Catalog>> = RwLock::new(None);
/// Parsed catalogs, kept so switching back and forth does not re-parse.
static PARSED: Mutex<Vec<(&'static str, &'static Catalog)>> = Mutex::new(Vec::new());
static EMPTY: std::sync::LazyLock<Catalog> = std::sync::LazyLock::new(HashMap::new);

/// Every language the app can show, as (code, native name), English first.
pub fn available_languages() -> Vec<(&'static str, &'static str)> {
    let name = |code: &'static str| {
        NATIVE_NAMES.iter().find(|(c, _)| *c == code).map_or(code, |(_, n)| *n)
    };
    std::iter::once("en")
        .chain(CATALOGS.iter().map(|(code, _)| *code))
        .map(|code| (code, name(code)))
        .collect()
}

/// Switches the language; `None` goes back to the desktop language.
pub fn set_language(code: Option<&str>) {
    let lang = match code {
        Some(code) => CATALOGS.iter().find(|(l, _)| *l == code).map(|(l, _)| *l),
        None => pick_language(),
    };
    *ACTIVE.write().unwrap() = Some(catalog_for(lang));
}

fn catalog_for(lang: Option<&'static str>) -> &'static Catalog {
    let Some(lang) = lang else { return &EMPTY };
    let mut parsed = PARSED.lock().unwrap();
    if let Some((_, catalog)) = parsed.iter().find(|(l, _)| *l == lang) {
        return catalog;
    }
    let po = CATALOGS.iter().find(|(l, _)| *l == lang).map_or("", |(_, po)| *po);
    let catalog: &'static Catalog = Box::leak(Box::new(parse_po(po)));
    parsed.push((lang, catalog));
    catalog
}

/// Marks a constant for extraction by xgettext without translating it yet.
pub const fn n_(msgid: &'static str) -> &'static str {
    msgid
}

pub fn tr(msgid: &'static str) -> &'static str {
    catalog().get(msgid).copied().unwrap_or(msgid)
}

/// Translates, then fills each `{}` in order with the arguments.
pub fn trf(msgid: &'static str, args: &[&dyn Display]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut rest = tr(msgid);
    while let Some(i) = rest.find("{}") {
        out.push_str(&rest[..i]);
        if let Some(arg) = args.next() {
            let _ = write!(out, "{arg}");
        }
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    out
}

fn catalog() -> &'static Catalog {
    if let Some(catalog) = *ACTIVE.read().unwrap() {
        return catalog;
    }
    let catalog = catalog_for(pick_language());
    *ACTIVE.write().unwrap() = Some(catalog);
    catalog
}

fn has_catalog(lang: &str) -> bool {
    CATALOGS.iter().any(|(l, _)| *l == lang)
}

fn pick_language() -> Option<&'static str> {
    for name in glib::language_names() {
        let locale = name.as_str().split(['.', '@']).next().unwrap_or_default();
        let language = locale.split('_').next().unwrap_or_default();
        for candidate in [locale, language] {
            if candidate == "en" || candidate == "C" || candidate == "POSIX" {
                return None;
            }
            if let Some((lang, _)) = CATALOGS.iter().find(|(l, _)| *l == candidate) {
                return Some(lang);
            }
        }
        if let Some((_, target)) = FALLBACKS.iter().find(|(from, _)| *from == language) {
            if has_catalog(target) {
                return CATALOGS.iter().find(|(l, _)| l == target).map(|(l, _)| *l);
            }
        }
    }
    None
}

fn unquote(line: &str) -> Option<String> {
    let inner = line.trim().strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    Some(out)
}

/// Minimal .po reader: msgid/msgstr pairs with continuation lines; fuzzy and
/// untranslated entries are skipped.
fn parse_po(text: &'static str) -> Catalog {
    enum Field {
        None,
        Id,
        Str,
    }
    let mut map = HashMap::new();
    let (mut id, mut value) = (String::new(), String::new());
    // A "#, fuzzy" comment precedes the msgid it belongs to.
    let (mut fuzzy, mut next_fuzzy) = (false, false);
    let mut field = Field::None;
    let mut flush = |id: &mut String, value: &mut String, fuzzy: bool| {
        if !id.is_empty() && !value.is_empty() && !fuzzy {
            let key: &'static str = Box::leak(std::mem::take(id).into_boxed_str());
            let val: &'static str = Box::leak(std::mem::take(value).into_boxed_str());
            map.insert(key, val);
        }
        id.clear();
        value.clear();
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("msgid ") {
            flush(&mut id, &mut value, fuzzy);
            fuzzy = std::mem::take(&mut next_fuzzy);
            id = unquote(rest).unwrap_or_default();
            field = Field::Id;
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            value = unquote(rest).unwrap_or_default();
            field = Field::Str;
        } else if line.starts_with('"') {
            let more = unquote(line).unwrap_or_default();
            match field {
                Field::Id => id.push_str(&more),
                Field::Str => value.push_str(&more),
                Field::None => {}
            }
        } else if line.starts_with("#,") && line.contains("fuzzy") {
            next_fuzzy = true;
        }
    }
    flush(&mut id, &mut value, fuzzy);
    map
}
