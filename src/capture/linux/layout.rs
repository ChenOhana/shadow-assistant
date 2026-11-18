//! The system's keyboard layout, for desktops that do not configure their own:
//! the session's `XKB_DEFAULT_*` environment, then systemd-localed.

use super::xkb::LayoutSpec;
use zbus::blocking::Connection;

/// The layout from `XKB_DEFAULT_*`, else systemd-localed, else `us`.
pub(super) fn system_layout(system: Option<&Connection>) -> LayoutSpec {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let from_env = LayoutSpec {
        layout: env("XKB_DEFAULT_LAYOUT"),
        variant: env("XKB_DEFAULT_VARIANT"),
        model: env("XKB_DEFAULT_MODEL"),
        options: env("XKB_DEFAULT_OPTIONS"),
    };
    if !from_env.layout.is_empty() {
        return from_env;
    }
    if let Some(spec) = system.and_then(layout_from_localed) {
        return spec;
    }
    LayoutSpec::default()
}

fn layout_from_localed(system: &Connection) -> Option<LayoutSpec> {
    let get = |prop: &str| -> Option<String> {
        let reply = system
            .call_method(
                Some("org.freedesktop.locale1"),
                "/org/freedesktop/locale1",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.freedesktop.locale1", prop),
            )
            .ok()?;
        let v: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
        String::try_from(v).ok()
    };
    let spec = LayoutSpec {
        layout: get("X11Layout")?,
        variant: get("X11Variant").unwrap_or_default(),
        model: get("X11Model").unwrap_or_default(),
        options: get("X11Options").unwrap_or_default(),
    };
    (!spec.layout.is_empty()).then_some(spec)
}
