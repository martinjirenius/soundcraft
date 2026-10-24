//! Installed LV2 plugins: discovery (`engine.lv2_plugins`). Hosting itself lives in
//! `soundcraft-lv2-host`; id lookup for the mixer commands is shared with CLAP in
//! `cmd::clap::plugin_info`.

use super::*;
use crate::cmd;
use serde_json::json;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        query "engine.lv2_plugins",
        "List LV2 Plugins",
        [],
        None,
        "{rescan?: bool} → [{id: \"lv2:<plugin URI>\", uri, name, vendor, category, is_instrument, classes, path}]",
        always,
        |_, p| {
            let list = if p.get("rescan").and_then(Value::as_bool).unwrap_or(false) {
                soundcraft_lv2_host::rescan()
            } else {
                soundcraft_lv2_host::scan()
            };
            Ok(json!(list))
        }
    )]
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    #[test]
    fn lv2_plugins_query_returns_a_list() {
        let mut e = Engine::default();
        let v = e.execute("engine.lv2_plugins", &json!({})).unwrap();
        assert!(v.is_array());
    }

    #[test]
    fn unknown_lv2_insert_is_an_error() {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        let r = e.execute("mix.insert", &json!({"track": t, "plugin": "lv2:urn:soundcraft:no-such-plugin"}));
        assert!(r.is_err());
        assert!(super::super::clap::plugin_info("lv2:urn:soundcraft:no-such-plugin").is_none());
    }
}
