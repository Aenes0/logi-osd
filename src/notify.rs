//! Avisos pelo daemon de notificações: cada `Pill` vira uma chamada `Notify` com as dicas da
//! pílula (`x-estilo osd`, `x-tom`, `x-pontos`…); a bateria baixa vira cartão. O daemon desenha.
//!
//! O cliente D-Bus é o `Channel` do libdbus no calloop, sem thread: `send` enfileira a chamada,
//! esvazia o buffer de saída e volta, sem esperar a resposta. Uma cópia (`dup`) do fd do watch
//! entra no laço como `Generic` (`READ`, nível) e só drena as respostas (descartadas; erro vira
//! uma linha de log por tipo até o próximo sucesso). Conexão caída é descartada; o próximo
//! `send` reconecta.

use std::cell::{Cell, RefCell};
use std::os::fd::BorrowedFd;
use std::rc::Rc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use dbus::Message;
use dbus::arg::{IterAppend, Variant};
use dbus::channel::{BusType, Channel};
use dbus::message::MessageType;
use dbus::strings::Signature;

use crate::message::{Icon, Pill, Tone, Value};

pub const APP_NAME: &str = "logi-osd";
/// Chave `synchronous`: o daemon troca a pílula anterior do logi-osd pela nova (nunca empilha).
pub const SYNC_KEY: &str = "logi-osd";

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const IFACE: &str = "org.freedesktop.Notifications";

/// Tipos de erro locais (os de resposta usam o nome do erro D-Bus).
const KIND_CONNECT: &str = "conexão";
const KIND_SEND: &str = "envio";

#[derive(Debug, Clone, PartialEq)]
pub enum Hint {
    Bool(bool),
    Int(i32),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotifyArgs {
    pub app_name: &'static str,
    pub app_icon: &'static str,
    pub summary: String,
    pub body: String,
    pub hints: Vec<(&'static str, Hint)>,
    pub expire_timeout: i32,
}

fn icon_name(icon: Icon) -> &'static str {
    match icon {
        Icon::CapsLock => "logi-caps-lock",
        Icon::NumLock => "logi-num-lock",
        Icon::Insert => "logi-insert",
        Icon::FnLock => "logi-fn-lock",
        Icon::KeyboardBacklight => "logi-backlight",
        Icon::Battery => "logi-battery",
        Icon::Charging => "logi-battery-charging",
        Icon::Switch => "logi-switch",
    }
}

fn tom(tone: Tone) -> &'static str {
    match tone {
        Tone::Accent => "normal",
        Tone::Dim => "apagado",
        Tone::Warning => "atencao",
        Tone::Critical => "critico",
    }
}

fn str_hint(key: &'static str, value: &str) -> (&'static str, Hint) {
    (key, Hint::Str(value.to_owned()))
}

/// Mapeamento puro da pílula para os argumentos do `Notify` (tabela do brief da integração T3).
pub fn to_notify(p: &Pill) -> NotifyArgs {
    let (body, value_tom) = match &p.value {
        Value::Text(text, tone) => (text.clone(), Some(tom(*tone))),
        Value::None => (String::new(), Some(tom(Tone::Dim))),
        Value::Dots { .. } => (String::new(), None),
    };

    // Bateria baixa/crítica: cartão (sem `x-estilo`, sem `transient`), fica no histórico.
    if p.icon == Icon::Battery {
        return NotifyArgs {
            app_name: APP_NAME,
            app_icon: icon_name(p.icon),
            summary: p.label.clone(),
            body,
            hints: vec![
                str_hint("x-tom", tom(p.icon_tone)),
                str_hint("category", "device"),
                str_hint("sound-name", "battery-low"),
            ],
            expire_timeout: -1,
        };
    }

    let mut hints = vec![
        str_hint("x-estilo", "osd"),
        ("transient", Hint::Bool(true)),
        str_hint("synchronous", SYNC_KEY),
    ];
    if let Some(t) = value_tom {
        hints.push(str_hint("x-tom", t));
    }
    if let Value::Dots { on, total } = p.value {
        hints.push(("x-pontos", Hint::Int(i32::from(on))));
        hints.push(("x-pontos-total", Hint::Int(i32::from(total))));
    }
    NotifyArgs {
        app_name: APP_NAME,
        app_icon: icon_name(p.icon),
        summary: p.label.clone(),
        body,
        hints,
        expire_timeout: i32::try_from(p.duration.as_millis()).unwrap_or(i32::MAX),
    }
}

/// Chamada `Notify` `(s u s s s as a{sv} i)`, com resposta esperada (a Central casa chamada e resposta).
fn notify_message(a: &NotifyArgs) -> Result<Message, String> {
    let mut m = Message::new_method_call(DEST, PATH, IFACE, "Notify")?;
    let mut it = IterAppend::new(&mut m);
    it.append(a.app_name);
    it.append(0u32);
    it.append(a.app_icon);
    it.append(a.summary.as_str());
    it.append(a.body.as_str());
    it.append(Vec::<String>::new());
    it.append_dict(&Signature::make::<&str>(), &Signature::make::<Variant<bool>>(), |d| {
        for (key, hint) in &a.hints {
            d.append_dict_entry(|e| {
                e.append(*key);
                match hint {
                    Hint::Bool(b) => e.append(Variant(*b)),
                    Hint::Int(i) => e.append(Variant(*i)),
                    Hint::Str(s) => e.append(Variant(s.as_str())),
                }
            });
        }
    });
    it.append(a.expire_timeout);
    Ok(m)
}

/// Uma linha de log por tipo de erro até o próximo sucesso (sem inundar o journal a cada tecla).
#[derive(Debug, Default)]
struct ErrorLog {
    seen: Vec<String>,
}

impl ErrorLog {
    /// Loga se `kind` ainda não foi logado desde o último sucesso; devolve se logou.
    fn report(&mut self, kind: &str, detail: &str) -> bool {
        if self.seen.iter().any(|k| k == kind) {
            return false;
        }
        self.seen.push(kind.to_owned());
        eprintln!("logi-osd: {detail}");
        true
    }

    fn success(&mut self) {
        self.seen.clear();
    }
}

/// Conexão viva, compartilhada entre o `Notifier` e o callback do fd no laço.
struct Conn {
    channel: Channel,
    log: Rc<RefCell<ErrorLog>>,
    dead: Cell<bool>,
    /// `None` depois que a fonte saiu do laço (pelo callback ou pelo `Notifier`).
    token: Cell<Option<RegistrationToken>>,
}

fn is_disconnected(msg: &Message) -> bool {
    msg.msg_type() == MessageType::Signal
        && msg.interface().is_some_and(|i| &*i == "org.freedesktop.DBus.Local")
        && msg.member().is_some_and(|m| &*m == "Disconnected")
}

impl Conn {
    /// Lê o socket sem bloquear e descarta as respostas; `false` se a conexão caiu.
    fn drain(&self) -> bool {
        let ch = &self.channel;
        if ch.read_write(Some(Duration::ZERO)).is_err() || !ch.is_connected() {
            self.dead.set(true);
        }
        while let Some(mut msg) = ch.pop_message() {
            match msg.msg_type() {
                MessageType::MethodReturn => self.log.borrow_mut().success(),
                MessageType::Error => {
                    if let Err(e) = msg.as_result() {
                        let name = e.name().unwrap_or("erro D-Bus").to_owned();
                        let detail = format!("aviso não entregue ao daemon de notificações ({name}): {}", e.message().unwrap_or(""));
                        self.log.borrow_mut().report(&name, &detail);
                    }
                }
                _ if is_disconnected(&msg) => self.dead.set(true),
                _ => {}
            }
        }
        !self.dead.get()
    }
}

pub struct Notifier {
    /// `None` = barramento de sessão do ambiente.
    address: Option<String>,
    conn: Option<Rc<Conn>>,
    log: Rc<RefCell<ErrorLog>>,
}

impl Notifier {
    /// `address`: None = barramento de sessão do ambiente; Some = endereço explícito (testes). Conecta na 1ª send.
    pub fn new(address: Option<String>) -> Notifier {
        Notifier { address, conn: None, log: Rc::new(RefCell::new(ErrorLog::default())) }
    }

    /// Não bloqueia esperando resposta: envia a chamada (com resposta esperada) e volta; as
    /// respostas e erros chegam pelo fd no calloop e são descartados (erro: uma linha de log por
    /// tipo até o próximo sucesso). Falha de envio descarta a conexão; a próxima send reconecta.
    pub fn send<S: 'static>(&mut self, handle: &LoopHandle<'static, S>, p: &Pill) {
        let msg = match notify_message(&to_notify(p)) {
            Ok(m) => m,
            Err(e) => {
                self.log.borrow_mut().report(KIND_SEND, &format!("chamada Notify não montada: {e}"));
                return;
            }
        };
        if self.conn.as_ref().is_some_and(|c| c.dead.get()) {
            self.disconnect(handle);
        }
        let Some(conn) = self.connection(handle) else { return };
        let queued = conn.channel.send(msg).is_ok();
        // Bloqueia só até o buffer de saída esvaziar (socket local), nunca pela resposta.
        conn.channel.flush();
        if !queued || !conn.channel.is_connected() {
            self.log
                .borrow_mut()
                .report(KIND_SEND, "a conexão com o barramento de sessão caiu; o próximo aviso reconecta");
            self.disconnect(handle);
            return;
        }
        // O `flush` pode ter lido mensagens para o buffer sem o fd voltar a ficar legível.
        if !conn.drain() {
            self.disconnect(handle);
        }
    }

    /// A conexão atual, ou uma nova registrada no laço; `None` (já logado) se não conectou.
    fn connection<S: 'static>(&mut self, handle: &LoopHandle<'static, S>) -> Option<Rc<Conn>> {
        if let Some(conn) = &self.conn {
            return Some(conn.clone());
        }
        match self.connect(handle) {
            Ok(conn) => {
                self.conn = Some(conn.clone());
                Some(conn)
            }
            Err(e) => {
                self.log.borrow_mut().report(KIND_CONNECT, &format!("sem barramento de sessão: {e}"));
                None
            }
        }
    }

    fn connect<S: 'static>(&self, handle: &LoopHandle<'static, S>) -> Result<Rc<Conn>, String> {
        let mut channel = match &self.address {
            None => Channel::get_private(BusType::Session),
            Some(address) => Channel::open_private(address).and_then(|mut ch| ch.register().map(|()| ch)),
        }
        .map_err(|e| e.message().unwrap_or("falha ao conectar").to_owned())?;
        channel.set_watch_enabled(true);
        // SAFETY: o fd do watch está aberto enquanto o canal (vivo aqui) existir; só é duplicado.
        // A cópia é da fonte do laço: o libdbus fecha o dele quando a conexão cai.
        let fd = unsafe { BorrowedFd::borrow_raw(channel.watch().fd) }
            .try_clone_to_owned()
            .map_err(|e| format!("dup do fd do D-Bus: {e}"))?;
        let conn = Rc::new(Conn { channel, log: self.log.clone(), dead: Cell::new(false), token: Cell::new(None) });
        let in_loop = conn.clone();
        let token = handle
            .insert_source(Generic::new(fd, Interest::READ, Mode::Level), move |_, _, _: &mut S| {
                if in_loop.drain() {
                    Ok(PostAction::Continue)
                } else {
                    in_loop.token.set(None);
                    Ok(PostAction::Remove)
                }
            })
            .map_err(|e| format!("fd do D-Bus no laço: {}", e.error))?;
        conn.token.set(Some(token));
        Ok(conn)
    }

    fn disconnect<S: 'static>(&mut self, handle: &LoopHandle<'static, S>) {
        if let Some(conn) = self.conn.take()
            && let Some(token) = conn.token.take()
        {
            handle.remove(token);
        }
    }

    #[cfg(test)]
    fn connected(&self) -> bool {
        self.conn.as_ref().is_some_and(|c| !c.dead.get())
    }

    #[cfg(test)]
    fn logged(&self) -> Vec<String> {
        self.log.borrow().seen.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::battery::BatteryAlert;
    use crate::message::{self, Durations, LockKey};

    const fn ms(n: u64) -> Duration { Duration::from_millis(n) }
    const D: Durations = Durations { normal: ms(1500), critical: ms(4000) };
    const VOCAB: [&str; 8] = [
        "logi-caps-lock",
        "logi-num-lock",
        "logi-insert",
        "logi-fn-lock",
        "logi-backlight",
        "logi-battery",
        "logi-battery-charging",
        "logi-switch",
    ];

    fn hints(a: &NotifyArgs) -> BTreeMap<&'static str, Hint> {
        a.hints.iter().cloned().collect()
    }

    fn s(v: &str) -> Hint { Hint::Str(v.into()) }

    fn osd(tom: Option<&str>) -> BTreeMap<&'static str, Hint> {
        let mut m = BTreeMap::from([("x-estilo", s("osd")), ("transient", Hint::Bool(true)), ("synchronous", s("logi-osd"))]);
        if let Some(t) = tom {
            m.insert("x-tom", s(t));
        }
        m
    }

    fn card(tom: &str) -> BTreeMap<&'static str, Hint> {
        BTreeMap::from([("x-tom", s(tom)), ("category", s("device")), ("sound-name", s("battery-low"))])
    }

    #[test]
    fn caps_on_maps_to_osd_pill() {
        let a = to_notify(&message::lock(LockKey::Caps, true, &D));
        assert_eq!((a.app_name, a.app_icon, a.summary.as_str(), a.body.as_str()), ("logi-osd", "logi-caps-lock", "Caps Lock", "Ativado"));
        assert_eq!(hints(&a), osd(Some("normal")));
        assert_eq!(a.expire_timeout, 1500);
    }

    #[test]
    fn caps_off_is_apagado() {
        let a = to_notify(&message::lock(LockKey::Caps, false, &D));
        assert_eq!((a.app_icon, a.body.as_str()), ("logi-caps-lock", "Desativado"));
        assert_eq!(hints(&a), osd(Some("apagado")));
        let num = to_notify(&message::lock(LockKey::Num, true, &D));
        assert_eq!((num.app_icon, num.summary.as_str()), ("logi-num-lock", "Num Lock"));
        let ins = to_notify(&message::lock(LockKey::Insert, false, &D));
        assert_eq!((ins.app_icon, ins.summary.as_str()), ("logi-insert", "Insert"));
    }

    #[test]
    fn fn_lock_texts_and_icon() {
        let f = to_notify(&message::fn_lock(false, &D));
        assert_eq!((f.app_icon, f.summary.as_str(), f.body.as_str()), ("logi-fn-lock", "Fn", "Teclas F"));
        assert_eq!(hints(&f), osd(Some("apagado")));
        assert_eq!(to_notify(&message::fn_lock(true, &D)).body, "Teclas de mídia");
        assert_eq!(f.expire_timeout, 1500);
    }

    #[test]
    fn backlight_maps_dots() {
        let a = to_notify(&message::backlight(3, 9, &D));
        assert_eq!((a.app_icon, a.summary.as_str(), a.body.as_str()), ("logi-backlight", "Teclado", ""));
        let mut want = osd(None);
        want.insert("x-pontos", Hint::Int(3));
        want.insert("x-pontos-total", Hint::Int(8));
        assert_eq!(hints(&a), want);
        assert_eq!(a.expire_timeout, 1500);
    }

    #[test]
    fn battery_low_is_atencao_card() {
        let a = to_notify(&message::battery(BatteryAlert::Low(15), "MX Keys Mini", &D));
        assert_eq!((a.app_icon, a.summary.as_str(), a.body.as_str()), ("logi-battery", "MX Keys Mini: bateria baixa", "15%"));
        assert_eq!(hints(&a), card("atencao"));
        assert_eq!(a.expire_timeout, -1);
    }

    #[test]
    fn battery_critical_is_critico_card() {
        let a = to_notify(&message::battery(BatteryAlert::Critical(5), "MX Keys Mini", &D));
        assert_eq!((a.app_icon, a.summary.as_str(), a.body.as_str()), ("logi-battery", "MX Keys Mini: bateria baixa", "5%"));
        assert_eq!(hints(&a), card("critico"));
        assert_eq!(a.expire_timeout, -1);
    }

    #[test]
    fn charging_and_charged_are_pills() {
        let c = to_notify(&message::battery(BatteryAlert::Charging(42), "G602", &D));
        assert_eq!((c.app_icon, c.summary.as_str(), c.body.as_str()), ("logi-battery-charging", "G602 carregando", "42%"));
        assert_eq!(hints(&c), osd(Some("apagado")));
        let d = to_notify(&message::battery(BatteryAlert::Charged, "G602", &D));
        assert_eq!((d.app_icon, d.summary.as_str(), d.body.as_str()), ("logi-battery-charging", "G602 carregado", ""));
        assert_eq!(hints(&d), osd(Some("apagado")));
        assert_eq!((c.expire_timeout, d.expire_timeout), (1500, 1500));
    }

    #[test]
    fn connected_with_and_without_channel() {
        let a = to_notify(&message::connected("MX Keys Mini", Some((3, 0)), &D));
        assert_eq!((a.app_icon, a.summary.as_str(), a.body.as_str()), ("logi-switch", "MX Keys Mini conectado", "Canal 1"));
        assert_eq!(hints(&a), osd(Some("apagado")));
        let b = to_notify(&message::connected("G602", None, &D));
        assert_eq!((b.summary.as_str(), b.body.as_str()), ("G602 conectado", ""));
        assert_eq!(hints(&b), osd(Some("apagado")));
    }

    #[test]
    fn every_demo_pill_maps() {
        let pills = crate::demo_pills(&D);
        assert_eq!(pills.len(), 10);
        for p in &pills {
            let a = to_notify(p);
            assert!(VOCAB.contains(&a.app_icon), "ícone fora do vocabulário: {}", a.app_icon);
            assert_eq!(a.app_name, "logi-osd");
            assert!(!a.summary.is_empty());
            assert!(notify_message(&a).is_ok());
        }
    }

    #[test]
    fn duration_in_ms_saturates() {
        let mut p = message::lock(LockKey::Caps, true, &D);
        p.duration = Duration::from_secs(u64::MAX / 4);
        assert_eq!(to_notify(&p).expire_timeout, i32::MAX);
    }

    #[test]
    fn error_log_once_per_kind_until_success() {
        let mut log = ErrorLog::default();
        assert!(log.report("a", "x"));
        assert!(!log.report("a", "x"));
        assert!(log.report("b", "y"));
        log.success();
        assert!(log.report("a", "x"));
    }

    mod bus_tests {
        use std::sync::mpsc::Receiver;
        use std::time::Instant;

        use calloop::EventLoop;

        use super::super::*;
        use super::D;
        use crate::message::{self, LockKey};
        use crate::test_bus::{PrivateBus, Recorded, Recorder};

        const WAIT: Duration = Duration::from_secs(2);
        const FAST: Duration = Duration::from_millis(50);

        fn event_loop() -> EventLoop<'static, ()> {
            EventLoop::try_new().expect("laço")
        }

        /// Roda o laço até a próxima chamada gravada (≤ 2 s).
        fn next_call(el: &mut EventLoop<'static, ()>, calls: &Receiver<Recorded>) -> Option<Recorded> {
            let deadline = Instant::now() + WAIT;
            while Instant::now() < deadline {
                if let Ok(r) = calls.try_recv() {
                    return Some(r);
                }
                el.dispatch(Some(Duration::from_millis(10)), &mut ()).expect("dispatch");
            }
            calls.try_recv().ok()
        }

        fn pump(el: &mut EventLoop<'static, ()>, d: Duration) {
            let end = Instant::now() + d;
            while Instant::now() < end {
                el.dispatch(Some(Duration::from_millis(10)), &mut ()).expect("dispatch");
            }
        }

        #[test]
        fn notifier_sends_notify_to_bus() {
            let bus = PrivateBus::start();
            let rec = Recorder::start(&bus);
            let mut el = event_loop();
            let mut n = Notifier::new(Some(bus.address.clone()));
            let pill = message::lock(LockKey::Caps, true, &D);
            n.send(&el.handle(), &pill);
            let got = next_call(&mut el, &rec.calls).expect("o Notify não chegou ao servidor");
            let want = to_notify(&pill);
            assert_eq!(got.signature, "susssasa{sv}i");
            assert_eq!(got.app_name, want.app_name);
            assert_eq!(got.replaces_id, 0);
            assert_eq!(got.app_icon, want.app_icon);
            assert_eq!(got.summary, want.summary);
            assert_eq!(got.body, want.body);
            assert!(got.actions.is_empty());
            let want_hints: std::collections::BTreeMap<String, Hint> =
                want.hints.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
            assert_eq!(got.hints, want_hints);
            assert_eq!(got.expire_timeout, 1500);
            // A resposta chega pelo fd e é drenada (sucesso limpa o log de erros).
            pump(&mut el, Duration::from_millis(100));
            assert!(n.connected());
            assert!(n.logged().is_empty());
        }

        #[test]
        fn send_without_daemon_does_not_block() {
            let bus = PrivateBus::start();
            let mut el = event_loop();
            let mut n = Notifier::new(Some(bus.address.clone()));
            let pill = message::lock(LockKey::Caps, true, &D);
            for _ in 0..3 {
                let t = Instant::now();
                n.send(&el.handle(), &pill);
                assert!(t.elapsed() < FAST, "send levou {:?}", t.elapsed());
            }
            // O erro do barramento (nome sem dono) chega pelo fd, é logado uma vez e a conexão segue.
            pump(&mut el, Duration::from_millis(200));
            assert!(n.connected());
            assert_eq!(n.logged(), vec!["org.freedesktop.DBus.Error.ServiceUnknown".to_string()]);
        }

        /// Daemon trocando de dono (crash, `dev.sh parar`, volta do oficial): o barramento segue de
        /// pé, então a mesma conexão serve; o aviso vai para quem for o dono na hora.
        #[test]
        fn follows_name_owner_change() {
            let bus = PrivateBus::start();
            let mut el = event_loop();
            let mut n = Notifier::new(Some(bus.address.clone()));
            {
                let rec = Recorder::start(&bus);
                n.send(&el.handle(), &message::lock(LockKey::Caps, true, &D));
                assert!(next_call(&mut el, &rec.calls).is_some(), "o primeiro Notify não chegou");
            }
            // Sem dono: erro logado, conexão mantida.
            bus.wait_name_released();
            n.send(&el.handle(), &message::lock(LockKey::Caps, false, &D));
            pump(&mut el, Duration::from_millis(100));
            assert!(n.connected());
            assert_eq!(n.logged(), vec!["org.freedesktop.DBus.Error.ServiceUnknown".to_string()]);

            let rec = Recorder::start(&bus);
            n.send(&el.handle(), &message::fn_lock(true, &D));
            let got = next_call(&mut el, &rec.calls).expect("o Notify para o dono novo não chegou");
            assert_eq!(got.body, "Teclas de mídia");
            pump(&mut el, Duration::from_millis(100));
            assert!(n.logged().is_empty());
        }

        #[test]
        fn reconnects_after_bus_error() {
            let socket = std::env::temp_dir().join(format!("logi-osd-teste-{}.sock", std::process::id()));
            let mut bus = PrivateBus::start_at(&socket);
            let mut el = event_loop();
            let mut n = Notifier::new(Some(bus.address.clone()));
            {
                let rec = Recorder::start(&bus);
                n.send(&el.handle(), &message::lock(LockKey::Caps, true, &D));
                assert!(next_call(&mut el, &rec.calls).is_some(), "o primeiro Notify não chegou");
            }

            bus.kill();
            let t = Instant::now();
            n.send(&el.handle(), &message::lock(LockKey::Caps, false, &D));
            assert!(t.elapsed() < FAST, "send com o barramento morto levou {:?}", t.elapsed());
            pump(&mut el, Duration::from_millis(100));
            assert!(!n.connected(), "a conexão morta não foi descartada");
            // Sem barramento: a tentativa de reconectar falha rápido, loga e segue.
            let t = Instant::now();
            n.send(&el.handle(), &message::lock(LockKey::Caps, false, &D));
            assert!(t.elapsed() < FAST, "reconectar sem barramento levou {:?}", t.elapsed());
            assert!(!n.logged().is_empty());

            let bus = PrivateBus::start_at(&socket);
            let rec = Recorder::start(&bus);
            n.send(&el.handle(), &message::lock(LockKey::Num, true, &D));
            let got = next_call(&mut el, &rec.calls).expect("o Notify depois da volta do barramento não chegou");
            assert_eq!(got.summary, "Num Lock");
            pump(&mut el, Duration::from_millis(100));
            assert!(n.connected());
            assert!(n.logged().is_empty(), "o sucesso não limpou o log: {:?}", n.logged());
            drop(bus);
            let _ = std::fs::remove_file(&socket);
        }
    }
}
