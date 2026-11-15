//! The console's Settings page: what the owner changes while the server
//! runs (see [`crate::server::settings`]), the memory of each application,
//! and, for the record, what is read once when the server starts.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::{Cx, Lang, app_base, bytes, esc, gigas, note};
use crate::server::State;
use crate::server::apps::App;
use crate::server::settings::{Kind, Refusal, Source, def, default_text, parse_mib, refused};

/// The form every "back to the environment's value" button belongs to (a
/// form cannot sit inside another one).
const RESET_FORM: &str = "settings-reset";

/// An amount of memory as a field shows it and an owner would type it:
/// `512 MiB`, `2 GiB`, `1.5 GiB` (in GiB when that says it exactly).
fn amount_text(l: Lang, mib: u64) -> String {
    let (m, g) = if l == Lang::Fr { ("Mio", "Gio") } else { ("MiB", "GiB") };
    if mib >= 1024 && mib.checked_mul(100).is_some_and(|m| m.is_multiple_of(1024)) {
        let hundredths = mib * 100 / 1024;
        let text = match (hundredths / 100, hundredths % 100) {
            (whole, 0) => whole.to_string(),
            (whole, part) if part % 10 == 0 => format!("{whole}.{}", part / 10),
            (whole, part) => format!("{whole}.{part:02}"),
        };
        format!("{} {g}", if l == Lang::En { text } else { text.replace('.', ",") })
    } else {
        format!("{mib} {m}")
    }
}

/// The value of a setting as its one spelling (`AUTO` → `auto`), for two to be compared.
fn spelled(key: &str, value: &str) -> String {
    def(key).and_then(|d| d.kind.check(key, value).ok()).unwrap_or_else(|| value.trim().to_owned())
}

/// What a field holds now: the value in force, as it would be typed.
fn shown(state: &State, cx: &Cx, key: &str) -> String {
    let value = state.settings.value(key).0.unwrap_or_else(|| default_text(&state.options, key));
    match def(key).map(|d| d.kind) {
        // Only a value this version accepts is written as an amount; anything else, as it was given.
        Some(Kind::Mib { .. }) => match spelled(key, &value).parse::<u64>() {
            Ok(mib) => amount_text(cx.l, mib),
            Err(_) => spelled(key, &value),
        },
        _ => value,
    }
}

/// The line under a field: where its value comes from, and the button that
/// gives it back.
fn source_line(state: &State, cx: &Cx, key: &str) -> String {
    let Some(d) = def(key) else { return String::new() };
    let env = state.settings.environment(key);
    let (_, source) = state.settings.value(key);
    let text = match (source, &env) {
        (Source::Saved, Some(e)) => {
            cx.tf("Saved here: it wins over the environment (<code>{}={}</code>).", &[&d.var, &esc(e)])
        }
        (Source::Saved, None) => cx.tf("Saved here (<code>{}</code> is not set in the environment).", &[&d.var]),
        (Source::Environment, Some(e)) => cx.tf("From the environment (<code>{}={}</code>).", &[&d.var, &esc(e)]),
        _ => cx.tf("Default (<code>{}</code> is not set).", &[&d.var]),
    };
    let mut out = format!(r#"<small class="muted">{text}"#);
    if let Some(lower) = state.settings.memory_capped().filter(|_| key == "memory.total_mb" && source == Source::Saved)
    {
        let _ = write!(
            out,
            " {}",
            cx.tf(
                "The environment's figure is lower: {} is in force (a saved memory only lowers it).",
                &[&amount_text(cx.l, lower)]
            )
        );
    }
    if source == Source::Saved && !state.settings.read_only() {
        let _ = write!(
            out,
            r#" <button class="plain" form="{RESET_FORM}" name="key" value="{key}">{}</button>"#,
            if env.is_some() { cx.t("Back to the environment's value") } else { cx.t("Back to the default") }
        );
    }
    out.push_str("</small>");
    out
}

/// A text field of a setting, with its help and where its value comes from.
fn field(state: &State, cx: &Cx, key: &str, label: &'static str, help: &str) -> String {
    format!(
        r#"<div class="set"><label>{label}<input name="{key}" value="{value}" autocomplete="off" spellcheck="false"{off}></label><small class="muted">{help}</small>{source}</div>"#,
        label = cx.t(label),
        value = esc(&shown(state, cx, key)),
        off = if state.settings.read_only() { " disabled" } else { "" },
        source = source_line(state, cx, key)
    )
}

fn save_button(state: &State, cx: &Cx, label: &'static str) -> String {
    if state.settings.read_only() { String::new() } else { format!(r#"<p><button>{}</button></p>"#, cx.t(label)) }
}

/// An amount of memory of an application, as its field shows it.
fn amount(l: Lang, bytes: usize) -> String {
    if bytes == 0 { "auto".into() } else { amount_text(l, bytes as u64 >> 20) }
}

/// The body of `/admin/settings`.
pub(super) fn body(state: &State, cx: &Cx, message: Option<String>) -> String {
    let l = cx.l;
    let read_only = state.settings.read_only();
    let mut body = format!(r#"<h1>{}</h1>"#, cx.t("Settings"));
    body.push_str(&message.unwrap_or_default());
    let _ = write!(
        body,
        r#"<p class="muted">{}</p><form id="{RESET_FORM}" method="post" action="/admin/settings/reset">{}</form>"#,
        if read_only {
            cx.t("The settings of this server are those of its environment (<code>NUAGE_SETTINGS=environment</code>): they are shown here, and change there.")
        } else {
            cx.t("What you save here applies at once and is kept with the data. It wins over the environment (the settings file, the host's variables); each line says where its value comes from, and one button gives it back.")
        },
        cx.field()
    );

    // What was saved and is not in force: shown, and removable.
    let ignored = state.settings.ignored();
    let kept: Vec<_> = state.apps.all().into_iter().filter(|a| a.kept_amounts() != (0, 0)).collect();
    if !ignored.is_empty() || !kept.is_empty() {
        let _ = write!(body, r#"<h2>{}</h2><div class="scroll"><table><tbody>"#, cx.t("Saved, and ignored"));
        for (key, value) in &ignored {
            let _ = write!(
                body,
                r#"<tr><td><code>{key}</code></td><td><code>{}</code></td><td><button class="plain" form="{RESET_FORM}" name="key" value="{key}">{}</button></td></tr>"#,
                esc(value),
                cx.t("Remove")
            );
        }
        for app in &kept {
            let (cache, kv) = app.kept_amounts();
            let said = |b: usize| if b == 0 { "auto".to_owned() } else { amount_text(cx.l, b as u64 >> 20) };
            let _ = write!(
                body,
                r#"<tr><td colspan="2">{}</td><td><form method="post" action="{}/memory">{}<input type="hidden" name="cache_mb" value="auto"><input type="hidden" name="kv_mb" value="auto"><input type="hidden" name="back" value="settings"><button class="plain">{}</button></form></td></tr>"#,
                esc(&cx.tf("application {}: page cache {}, key-value store {}", &[&app.name, &said(cache), &said(kv)])),
                app_base(&app.name),
                cx.field(),
                cx.t("Remove")
            );
        }
        body.push_str("</tbody></table></div>");
    }

    // Memory: the server's, then each application's.
    let memory = crate::memory::MemoryInfo::now();
    let detected = crate::memory::detected().total_bytes;
    let _ = write!(body, r#"<h2 id="memory">{}</h2><p>"#, cx.t("Memory"));
    match (memory.total_bytes, memory.used_bytes, memory.available_bytes) {
        (Some(total), Some(used), Some(free)) => body.push_str(
            &cx.tf("This server may use {}: {} used, {} free.", &[&gigas(l, total), &gigas(l, used), &gigas(l, free)]),
        ),
        _ => body.push_str(cx.t("This system does not tell Nuage DB its memory: say how much it may count on.")),
    }
    let budget = state.apps.cache_budget();
    let _ = write!(
        body,
        r#"</p><p class="muted">{}</p><form method="post" action="/admin/settings/memory">{}"#,
        cx.t("Amounts are written <code>auto</code>, <code>512 MiB</code> or <code>2 GiB</code> (a number alone is in MiB)."),
        cx.field()
    );
    body.push_str(&field(
        state,
        cx,
        "memory.total_mb",
        "Memory Nuage DB may count on",
        &cx.tf(
            "<code>auto</code>: what the system reports ({}), or <code>NUAGE_MEMORY_MB</code> when it is set.",
            &[&detected.map_or_else(|| cx.t("unknown").to_owned(), |d| gigas(l, d))],
        ),
    ));
    body.push_str(&field(
        state,
        cx,
        "memory.cache_mb",
        "Page caches, all the applications together",
        &cx.tf(
            "<code>auto</code> follows the memory: a quarter of it at first, up to six tenths while there is room, less when it gets tight. An amount is a maximum. Now: {} (up to {}).",
            &[
                &budget.map_or_else(|| cx.t("unknown").to_owned(), |b| bytes(l, b as u64)),
                &bytes(l, state.apps.cache_ceiling().unwrap_or(budget.unwrap_or(0)) as u64),
            ],
        ),
    ));
    let apps = state.apps.all();
    let kv_each = state.apps.kv_each();
    body.push_str(&field(
        state,
        cx,
        "memory.kv_mb",
        "Key-value store of each application",
        &cx.tf(
            "<code>auto</code>: a sixteenth of the memory each, an eighth for all of them together. Lowering it drops the least recently used keys at once. Now: {} each.",
            &[&bytes(l, kv_each as u64)],
        ),
    ));
    body.push_str(&save_button(state, cx, "Save the memory"));
    body.push_str("</form>");

    let _ = write!(
        body,
        r#"<h2 id="applications">{}</h2><p class="muted">{}</p>"#,
        cx.t("Memory by application"),
        cx.t("Each application shares the page caches of the server by use (<code>auto</code>), or has an amount set apart for it alone.")
    );
    // One form for each row, before the table: a form cannot wrap table cells.
    for app in &apps {
        let _ = write!(
            body,
            r#"<form id="memory-{name}" method="post" action="{base}/memory">{f}<input type="hidden" name="back" value="settings"></form>"#,
            name = esc(&app.name),
            base = app_base(&app.name),
            f = cx.field()
        );
    }
    let _ = write!(
        body,
        r#"<div class="scroll"><table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th>{}</th><th></th></tr></thead><tbody>"#,
        cx.t("Application"),
        cx.t("Its cache holds"),
        cx.t("may hold"),
        cx.t("Page cache"),
        cx.t("Key-value store")
    );
    for app in &apps {
        let name = esc(&app.name);
        let off = if read_only { " disabled" } else { "" };
        let _ = write!(
            body,
            r#"<tr><td><a href="{base}/settings">{name}</a></td><td>{holds}</td><td>{may}</td>
<td><input form="memory-{name}" name="cache_mb" value="{cache}" aria-label="{cache_label}" style="max-width:8rem"{off}></td>
<td><input form="memory-{name}" name="kv_mb" value="{kv}" aria-label="{kv_label}" style="max-width:8rem"{off}></td>
<td>{button}</td></tr>"#,
            base = app_base(&app.name),
            holds = bytes(l, app.engine.cache_used() as u64),
            may = bytes(l, app.engine.cache_capacity() as u64),
            cache = amount(l, app.cache_reserved()),
            kv = amount(l, app.kv_fixed()),
            cache_label = cx.t("Page cache"),
            kv_label = cx.t("Key-value store"),
            button = if read_only {
                String::new()
            } else {
                format!(r#"<button form="memory-{name}">{}</button>"#, cx.t("Save"))
            }
        );
    }
    body.push_str("</tbody></table></div>");
    let _ = write!(
        body,
        r#"<p class="muted phone">{}</p>"#,
        cx.t("On a phone, each application's Settings tab has the same fields, one under the other.")
    );
    let (apart, floor) = state.apps.cache_set_apart();
    if apart > 0 {
        let figures: [&dyn std::fmt::Display; 2] =
            [&bytes(l, apart as u64), &bytes(l, state.apps.cache_ceiling().unwrap_or(apart) as u64)];
        let said = if floor > 0 {
            cx.tf(
                "Set apart: {} of the {} the caches may take; the others share the rest by use. When memory runs short, what is set apart shrinks with the rest, and comes back.",
                &figures,
            )
        } else {
            cx.tf(
                "Set apart: {} of the {} the caches may take (every application has an amount of its own). When memory runs short, what is set apart shrinks with the rest, and comes back.",
                &figures,
            )
        };
        let _ = write!(body, r#"<p class="muted">{said}</p>"#);
        match state.apps.cache_squeezed() {
            Some((asked, has, true)) => body.push_str(&note(
                false,
                &cx.tf(
                    "Memory is short: what is set apart is cut to {} of {} for now.",
                    &[&bytes(l, has as u64), &bytes(l, asked as u64)],
                ),
            )),
            Some((asked, has, false)) => body.push_str(&note(
                false,
                &cx.tf(
                    "The memory was lowered since: what is set apart has {} of the {} asked, in proportion.",
                    &[&bytes(l, has as u64), &bytes(l, asked as u64)],
                ),
            )),
            None => {}
        }
    }

    // Limits.
    let _ = write!(
        body,
        r#"<h2 id="limits">{}</h2><form method="post" action="/admin/settings/limits">{}"#,
        cx.t("Limits"),
        cx.field()
    );
    body.push_str(&field(
        state,
        cx,
        "statement.memory_mb",
        "Memory of one statement",
        cx.t("What one SQL statement may hold at once (the rows it sorts, joins or returns); past it, the statement fails with a clear error instead of filling the memory. <code>auto</code>: an eighth of the memory."),
    ));
    body.push_str(&field(
        state,
        cx,
        "statement.seconds",
        "Seconds a statement may run",
        cx.t("Past it, the statement stops with an error and its changes are undone. <code>0</code>: no limit."),
    ));
    body.push_str(&field(
        state,
        cx,
        "postgres.connections",
        "PostgreSQL connections at once",
        cx.t("Each has a thread; the next ones are refused as PostgreSQL does."),
    ));
    body.push_str(&save_button(state, cx, "Save the limits"));
    body.push_str("</form>");

    // Visibility.
    let public = state.public_status();
    let _ = write!(
        body,
        r#"<h2 id="visibility">{title}</h2><form method="post" action="/admin/settings/visibility">{f}
<div class="set"><label class="check"><input type="checkbox" name="status.public" value="true"{checked}{off}> {label}</label>{source}</div>{save}</form>"#,
        title = cx.t("Visibility"),
        f = cx.field(),
        checked = if public { " checked" } else { "" },
        off = if read_only { " disabled" } else { "" },
        label =
            cx.t("Public status: <code>/api/status</code> and the dashboard show a public subset without signing in"),
        source = source_line(state, cx, "status.public"),
        save = save_button(state, cx, "Save the visibility")
    );

    // What is read once, at startup.
    let _ = write!(
        body,
        r#"<h2 id="startup">{}</h2><p class="muted">{} {}</p>"#,
        cx.t("Set when the server starts"),
        cx.t("The server reads these before it opens its data, or they are what it listens on and what opens it: a wrong value saved from here could lock you out. They change in the environment, then the server restarts."),
        match (&state.options.settings_file, std::env::var_os("DYNO").is_some()) {
            (Some(file), _) =>
                cx.tf("On this computer: edit <code>{}</code>, then restart the service.", &[&esc(file)]),
            (None, true) => cx.t("On Heroku: <code>heroku config:set NAME=value</code> (the server restarts).").to_owned(),
            (None, false) => cx
                .t("Change the variables of the environment (Docker: <code>.env</code>), then restart the server.")
                .to_owned(),
        }
    );
    let said: Vec<_> = state.options.startup.iter().filter(|s| s.value.is_some()).collect();
    if !said.is_empty() {
        let _ = write!(
            body,
            r#"<div class="scroll"><table><thead><tr><th>{}</th><th>{}</th></tr></thead><tbody>"#,
            cx.t("Variable"),
            cx.t("Value")
        );
        for s in said {
            let value = s.value.as_deref().unwrap_or_default();
            let _ = write!(
                body,
                "<tr><td><code>{}</code></td><td>{}</td></tr>",
                s.var,
                if value == crate::server::StartupSetting::SECRET {
                    format!(r#"<span class="pill">{}</span>"#, cx.t("set (a secret: not shown)"))
                } else {
                    format!("<code>{}</code>", esc(value))
                }
            );
        }
        body.push_str("</tbody></table></div>");
    }
    body
}

/// The keys of each form of the page.
fn keys_of(sub: &str) -> &'static [&'static str] {
    match sub {
        "/memory" => &["memory.total_mb", "memory.cache_mb", "memory.kv_mb"],
        "/limits" => &["statement.memory_mb", "statement.seconds", "postgres.connections"],
        "/visibility" => &["status.public"],
        _ => &[],
    }
}

/// A form of `/admin/settings` was sent: saves what changed, and says so.
/// Returns whether it went well, and the message to show above the page.
pub(super) fn post(state: &State, sub: &str, form: &HashMap<String, String>, cx: &Cx) -> (bool, String) {
    let sent = |key: &str| form.get(key).map(|v| v.trim().to_owned());
    let mut changes: Vec<(&str, Option<String>)> = Vec::new();
    if sub == "/reset" {
        match sent("key").as_deref().and_then(def) {
            Some(d) => changes.push((d.key, None)),
            None => return (false, note(false, cx.t("Unknown action."))),
        }
    } else {
        for &key in keys_of(sub) {
            // A box that is not ticked is not sent.
            let value = match (key, sent(key)) {
                ("status.public", v) => Some(v.is_some().to_string()),
                (_, v) => v,
            };
            let Some(value) = value else { continue };
            // What the field showed and was left as it was stays where it comes from.
            let in_force = state.settings.value(key).0.unwrap_or_else(|| default_text(&state.options, key));
            // Amounts are compared as amounts, whatever this version accepts.
            let same_amount = matches!(def(key).map(|d| d.kind), Some(Kind::Mib { .. }))
                && match (parse_mib(&value), parse_mib(&in_force)) {
                    (Some(a), Some(b)) => a == b,
                    _ => value.eq_ignore_ascii_case(&in_force),
                };
            if same_amount || spelled(key, &value) == spelled(key, &in_force) {
                continue;
            }
            // An emptied field gives the setting back.
            changes.push((key, Some(value).filter(|v| !v.is_empty())));
        }
        if keys_of(sub).is_empty() {
            return (false, note(false, cx.t("Unknown action.")));
        }
    }
    if changes.is_empty() {
        return (true, note(true, cx.t("Nothing changed.")));
    }
    let list: Vec<(&str, Option<&str>)> = changes.iter().map(|(k, v)| (*k, v.as_deref())).collect();
    match state.save_settings(&list) {
        Ok(unshipped) if sub == "/reset" => (
            true,
            note(true, &cx.tf("{} is given back: the value in force is shown below.", &[&changes[0].0]))
                + &not_in_the_backup(cx, unshipped),
        ),
        Ok(unshipped) => (true, note(true, cx.t("Settings saved: in force now.")) + &not_in_the_backup(cx, unshipped)),
        Err(refusal) => (false, note(false, &refusal.said(cx.l))),
    }
}

/// What is said when a change is saved and in force but did not reach the
/// backup: on a disk the host erases, a restart would undo it.
fn not_in_the_backup(cx: &Cx, unshipped: Option<String>) -> String {
    // The cause, in the system's words, is in the log (and in the API's answer).
    unshipped.map_or_else(String::new, |_| {
        note(
            false,
            cx.t("Not yet in the backup: the backup failed (the log says why). A restart of the server now would bring the earlier value back."),
        )
    })
}

/// The Memory section of an application's Settings tab.
pub(super) fn app_memory(state: &State, cx: &Cx, app: &App) -> String {
    let l = cx.l;
    let read_only = state.settings.read_only();
    let off = if read_only { " disabled" } else { "" };
    format!(
        r#"<h2>{title}</h2><p class="muted">{help} <a href="/admin/settings#memory">{all}</a></p>
<form method="post" action="{base}/memory" class="row">{f}
<label>{cache_label}<input name="cache_mb" value="{cache}" autocomplete="off"{off}></label>
<label>{kv_label}<input name="kv_mb" value="{kv}" autocomplete="off"{off}></label>{button}</form>"#,
        title = cx.t("Memory"),
        help = cx.tf(
            "This application shares the server's memory with the others (<code>auto</code>), or has an amount of its own (<code>64 MiB</code>, <code>2 GiB</code>). Its cache holds {} of {} now.",
            &[&bytes(l, app.engine.cache_used() as u64), &bytes(l, app.engine.cache_capacity() as u64)],
        ),
        all = cx.t("All the memory settings"),
        base = app_base(&app.name),
        f = cx.field(),
        cache_label = cx.t("Page cache"),
        kv_label = cx.t("Key-value store"),
        cache = amount(l, app.cache_reserved()),
        kv = amount(l, app.kv_fixed()),
        button = if read_only { String::new() } else { format!("<button>{}</button>", cx.t("Save the memory")) }
    )
}

/// An amount of memory as typed in a field: nothing or `auto` for the
/// server's rule, else an amount.
fn typed_amount(field: &str, typed: &str) -> Result<Option<usize>, Refusal> {
    let typed = typed.trim();
    if typed.is_empty() || typed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    match parse_mib(typed) {
        Some(mib) if mib <= 1 << 24 => Ok(Some((mib as usize) << 20)),
        Some(mib) => Err(Refusal::new(refused::TOO_MUCH_FOR_ANY, &[&field, &mib])),
        None => Err(Refusal::new(refused::NOT_AN_AMOUNT, &[&field, &typed])),
    }
}

/// The memory form of an application was sent.
pub(super) fn post_app_memory(state: &State, cx: &Cx, app: &App, form: &HashMap<String, String>) -> (bool, String) {
    let typed = |key: &str| form.get(key).map(String::as_str).unwrap_or_default();
    let amounts = typed_amount(cx.t("Page cache"), typed("cache_mb"))
        .and_then(|cache| typed_amount(cx.t("Key-value store"), typed("kv_mb")).map(|kv| (cache, kv)));
    // Where the environment rules, only removing what was kept aside is possible.
    let amounts =
        if state.settings.read_only() && matches!(amounts, Ok((None, None))) { Ok((None, None)) } else { amounts };
    match amounts.and_then(|(cache, kv)| state.set_app_memory(app, cache, kv)) {
        Ok((false, _)) => (true, note(true, cx.t("Nothing changed."))),
        Ok((true, unshipped)) => (
            true,
            note(true, &cx.tf("Memory of {} saved: in force now.", &[&app.name])) + &not_in_the_backup(cx, unshipped),
        ),
        Err(refusal) => (false, note(false, &refusal.said(cx.l))),
    }
}

/// Every key of the settings has a field on the page: a setting added to
/// [`DEFS`] without one would be out of reach from the console.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::settings::DEFS;

    #[test]
    fn every_setting_has_a_field() {
        let on_the_page: Vec<&str> =
            ["/memory", "/limits", "/visibility"].iter().flat_map(|sub| keys_of(sub).iter().copied()).collect();
        for d in &DEFS {
            assert!(on_the_page.contains(&d.key), "{} has no field", d.key);
        }
        assert_eq!(on_the_page.len(), DEFS.len());
    }
}
