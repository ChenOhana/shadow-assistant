//! D-Bus helpers shared by the desktop backends and logind.

use anyhow::Result;
use zbus::blocking::{Connection, MessageIterator};
use zbus::{MatchRule, message::Type as MsgType};

/// The unique bus name currently owning the well-known name `name`.
pub(super) fn name_owner(conn: &Connection, name: &str) -> Result<String> {
    let owner: String = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &(name,),
        )?
        .body()
        .deserialize()?;
    Ok(owner)
}

/// Spawns a thread that calls `handler` for each matching signal sent by the
/// current owner of the well-known name `sender`. Signals from anyone else
/// on the bus are dropped. `arg0` restricts on the first string argument.
pub(super) fn spawn_signal_watch(
    conn: &Connection,
    sender: &'static str,
    interface: &'static str,
    member: &'static str,
    arg0: Option<&'static str>,
    mut handler: impl FnMut(&zbus::Message) + Send + 'static,
) {
    let conn = conn.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("dbus-{member}"))
        .spawn(move || {
            let mut run = || -> Result<()> {
                let mut rule = MatchRule::builder()
                    .msg_type(MsgType::Signal)
                    .interface(interface)?
                    .member(member)?;
                if let Some(a) = arg0 {
                    rule = rule.add_arg(a)?;
                }
                let iter = MessageIterator::for_match_rule(rule.build(), &conn, Some(64))?;
                let mut owner = name_owner(&conn, sender).ok();
                for msg in iter {
                    let msg = msg?;
                    let from = msg.header().sender().map(|s| s.as_str().to_string());
                    if from.is_none() {
                        continue;
                    }
                    if from != owner {
                        // The service may have restarted under a new unique name.
                        owner = name_owner(&conn, sender).ok();
                        if from != owner {
                            continue; // spoofed: not sent by the service we trust
                        }
                    }
                    handler(&msg);
                }
                Ok(())
            };
            if let Err(e) = run() {
                eprintln!("brain: warning: stopped listening for {interface}.{member}: {e:#}");
            }
        });
    if let Err(e) = spawn {
        eprintln!("brain: warning: could not start D-Bus watcher thread: {e}");
    }
}
