//! Apoio dos testes de barramento (só `cfg(test)`): um `dbus-daemon` de sessão privado e um
//! servidor `org.freedesktop.Notifications` falso que grava as chamadas `Notify`.
//! Mesmo modelo do `PrivateBus` do daemon de notificações (`tests/common/mod.rs`).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::Duration;

use dbus::Message;
use dbus::arg::{PropMap, RefArg, cast};
use dbus::blocking::Connection;
use dbus::channel::Channel;
use dbus::message::MessageType;

use crate::notify::Hint;

pub const NAME: &str = "org.freedesktop.Notifications";

/// `dbus-daemon` de sessão só deste teste; morre no `Drop`.
pub struct PrivateBus {
    pub address: String,
    child: Child,
}

impl PrivateBus {
    /// Endereço escolhido pelo `dbus-daemon`.
    pub fn start() -> PrivateBus {
        PrivateBus::spawn(None)
    }

    /// Endereço fixo `unix:path=<socket>` (para derrubar e subir de novo no mesmo endereço).
    pub fn start_at(socket: &Path) -> PrivateBus {
        let _ = std::fs::remove_file(socket);
        PrivateBus::spawn(Some(format!("unix:path={}", socket.display())))
    }

    fn spawn(address: Option<String>) -> PrivateBus {
        let mut cmd = Command::new("dbus-daemon");
        cmd.args(["--session", "--nofork", "--print-address=1"]);
        if let Some(address) = &address {
            cmd.arg(format!("--address={address}"));
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon não iniciou");
        let stdout = child.stdout.take().expect("sem stdout do dbus-daemon");
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).expect("dbus-daemon não imprimiu o endereço");
        let printed = line.trim().to_string();
        assert!(!printed.is_empty(), "endereço vazio do dbus-daemon");
        // Com endereço fixo, devolve o pedido (o impresso traz `,guid=`, que muda a cada subida).
        PrivateBus { address: address.unwrap_or(printed), child }
    }

    /// Derruba o barramento (o `Drop` continua seguro).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Espera o nome ficar sem dono (≤ 2 s): a saída do dono é processada pelo barramento depois.
    pub fn wait_name_released(&self) {
        let client = self.client();
        let proxy = client.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", Duration::from_secs(2));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let owned: Result<(bool,), _> = proxy.method_call("org.freedesktop.DBus", "NameHasOwner", (NAME,));
            if matches!(owned, Ok((false,))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{NAME} ainda tem dono");
    }

    pub fn client(&self) -> Connection {
        let mut channel = Channel::open_private(&self.address).expect("cliente não conectou");
        channel.register().expect("cliente não registrou");
        Connection::from(channel)
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Uma chamada `Notify` como chegou ao servidor.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub signature: String,
    pub app_name: String,
    pub replaces_id: u32,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<String>,
    pub hints: BTreeMap<String, Hint>,
    pub expire_timeout: i32,
}

/// Servidor falso numa thread de teste: toma o nome, responde a `Notify` com um id e grava a
/// chamada. Para no `Drop`.
pub struct Recorder {
    pub calls: Receiver<Recorded>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    pub fn start(bus: &PrivateBus) -> Recorder {
        let conn = bus.client();
        conn.request_name(NAME, false, true, false).expect("RequestName do servidor falso");
        let (tx, calls) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut next_id = 1u32;
            while !stop_thread.load(Ordering::Relaxed) {
                let Ok(msg) = conn.channel().blocking_pop_message(Duration::from_millis(20)) else { return };
                let Some(msg) = msg else { continue };
                if msg.msg_type() != MessageType::MethodCall || msg.member().is_none_or(|m| &*m != "Notify") {
                    continue;
                }
                if let Some(rec) = parse(&msg) {
                    let _ = tx.send(rec);
                }
                let _ = conn.channel().send(msg.method_return().append1(next_id));
                conn.channel().flush();
                next_id += 1;
            }
        });
        Recorder { calls, stop, thread: Some(thread) }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn parse(msg: &Message) -> Option<Recorded> {
    let mut signature = String::new();
    let mut it = msg.iter_init();
    while it.arg_type() != dbus::arg::ArgType::Invalid {
        signature.push_str(&it.signature());
        it.next();
    }
    let (app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout): (
        String,
        u32,
        String,
        String,
        String,
        Vec<String>,
        PropMap,
        i32,
    ) = msg.read_all().ok()?;
    let hints = hints.into_iter().filter_map(|(k, v)| hint(&*v.0).map(|h| (k, h))).collect();
    Some(Recorded { signature, app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout })
}

fn hint(v: &(dyn RefArg + 'static)) -> Option<Hint> {
    if let Some(b) = cast::<bool>(v) {
        return Some(Hint::Bool(*b));
    }
    if let Some(i) = cast::<i32>(v) {
        return Some(Hint::Int(*i));
    }
    cast::<String>(v).map(|s| Hint::Str(s.clone()))
}
