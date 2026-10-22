mod app;
mod battery;
mod config;
mod devices;
mod emoji;
mod hidpp;
mod message;
mod notify;
#[cfg(test)]
mod test_bus;

use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use calloop::channel::{self, Sender};
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};

use crate::app::{Action, App, NodeEvent};
use crate::battery::BatteryAlert;
use crate::config::Config;
use crate::devices::node::{FoundNode, Hotplug, LiveNodes, is_new_device, reinspect, spawn_node};
use crate::message::{Durations, LockKey, Pill};
use crate::notify::Notifier;

/// Depuração (`--listar-nos`): quanto tempo imprimir os eventos dos nós.
const LIST_NODES_FOR: Duration = Duration::from_secs(30);
/// `--demo`: intervalo entre pílulas.
const DEMO_STEP: Duration = Duration::from_secs(2);
const DEMO_NAME: &str = "MX Keys Mini";

type BoxError = Box<dyn std::error::Error>;

#[derive(Clone, Copy, PartialEq)]
enum RunMode { Daemon, Demo }

fn main() -> ExitCode {
    let result = match std::env::args().nth(1).as_deref() {
        None => run(RunMode::Daemon),
        Some("--demo") => run(RunMode::Demo),
        Some("--listar-nos") => list_nodes(LIST_NODES_FOR).map_err(BoxError::from),
        Some(other) => Err(format!("opção desconhecida: {other} (use --demo ou --listar-nos)").into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("logi-osd: {e}");
            ExitCode::FAILURE
        }
    }
}

pub struct State {
    app: App,
    /// Os avisos vão ao daemon de notificações (que desenha a pílula e troca a anterior).
    notifier: Notifier,
    cfg: Config,
    handle: LoopHandle<'static, State>,
    live: LiveNodes,
    tx: Sender<NodeEvent>,
    /// `LOGI_OSD_DEBUG=1`: imprime `{ms} ms evento→envio`.
    debug: bool,
}

/// Repouso: nenhum timer armado; o loop dorme em `run(None)` até um evento de nó, de hotplug ou
/// uma resposta do barramento (só logo depois de um aviso enviado).
fn run(mode: RunMode) -> Result<(), BoxError> {
    let cfg = config::load();
    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let handle = event_loop.handle();

    let (tx, rx) = channel::channel::<NodeEvent>();
    handle
        .insert_source(rx, |ev, _, s: &mut State| {
            if let channel::Event::Msg(ev) = ev {
                s.on_node_event(ev);
            }
        })
        .map_err(|e| e.error)?;

    let mut state = State {
        app: App::new(&cfg),
        notifier: Notifier::new(None),
        cfg,
        handle: handle.clone(),
        live: LiveNodes::default(),
        tx,
        debug: std::env::var_os("LOGI_OSD_DEBUG").is_some_and(|v| v == "1"),
    };

    match mode {
        RunMode::Daemon => {
            let (hotplug, found) = Hotplug::new()?;
            for n in found {
                state.start_node(n, true);
            }
            handle
                .insert_source(Generic::new(hotplug, Interest::READ, Mode::Level), |_, hp, s: &mut State| {
                    for n in hp.drain() {
                        s.start_node(n, false);
                    }
                    Ok(PostAction::Continue)
                })
                .map_err(|e| e.error)?;
        }
        RunMode::Demo => {
            let pills = demo_pills(&Durations { normal: state.cfg.duration, critical: message::CRITICAL_DURATION });
            let signal = event_loop.get_signal();
            let mut next = 0;
            handle
                .insert_source(Timer::immediate(), move |_, _, s: &mut State| match pills.get(next) {
                    Some(pill) => {
                        s.show(pill.clone(), Instant::now());
                        next += 1;
                        TimeoutAction::ToDuration(DEMO_STEP)
                    }
                    None => {
                        signal.stop();
                        TimeoutAction::Drop
                    }
                })
                .map_err(|e| e.error)?;
        }
    }

    event_loop.run(None, &mut state, |_| {})?;
    Ok(())
}

/// Uma pílula de cada linha da tabela de cópia.
fn demo_pills(d: &Durations) -> Vec<Pill> {
    vec![
        message::lock(LockKey::Caps, true, d),
        message::lock(LockKey::Num, false, d),
        message::lock(LockKey::Insert, true, d),
        message::fn_lock(true, d),
        message::backlight(3, 9, d),
        message::battery(BatteryAlert::Low(15), DEMO_NAME, d),
        message::battery(BatteryAlert::Critical(5), DEMO_NAME, d),
        message::battery(BatteryAlert::Charging(42), DEMO_NAME, d),
        message::battery(BatteryAlert::Charged, DEMO_NAME, d),
        message::connected(DEMO_NAME, Some((3, 0)), d),
    ]
}

impl State {
    fn on_node_event(&mut self, ev: NodeEvent) {
        let gone = match ev {
            NodeEvent::NodeGone { node } => Some(node),
            _ => None,
        };
        let now = Instant::now();
        match self.app.handle(ev, now) {
            Some(Action::Show(pill)) => self.show(pill, now),
            Some(Action::RunEmojiCommand) => self.run_emoji(),
            None => {}
        }
        if let Some(node) = gone {
            self.on_node_gone(node);
        }
    }

    fn run_emoji(&self) {
        // `Config` garante: `Some` = comando não vazio (mesmo predicado do `App` e do desvio).
        if let Some(cmd) = self.cfg.emoji_command.as_deref()
            && let Err(e) = emoji::run(cmd)
        {
            eprintln!("logi-osd: comando emoji \"{cmd}\": {e}");
        }
    }

    /// Solta o devnode; se ele ainda existe e é outro dispositivo (hidrawN reaproveitado, cujo
    /// `add` do udev foi ignorado porque o nó antigo ainda vivia), reinicia.
    fn on_node_gone(&mut self, node: u32) {
        let Some((devnode, stamp)) = self.live.gone(node) else { return };
        if let Some(found) = reinspect(&devnode)
            && is_new_device(stamp, &found)
        {
            self.start_node(found, false);
        }
    }

    fn start_node(&mut self, n: FoundNode, initial_enumeration: bool) {
        let Some(id) = self.live.admit(&n) else { return };
        if let Err(e) = spawn_node(n.devnode.clone(), id, n.kind, n.product, initial_enumeration, &self.cfg, self.tx.clone()) {
            eprintln!("logi-osd: {}: {e}", n.devnode.display());
            self.live.gone(id);
        }
    }

    /// `now` = instante do evento (base da medição do `LOGI_OSD_DEBUG`).
    fn show(&mut self, pill: Pill, now: Instant) {
        self.notifier.send(&self.handle, &pill);
        if self.debug {
            eprintln!("logi-osd: {:.1} ms evento→envio", now.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

/// Imprime os nós selecionados e os `NodeEvent` recebidos por `duration` (hotplug incluído).
fn list_nodes(duration: Duration) -> io::Result<()> {
    let cfg = config::load();
    let (hotplug, found) = Hotplug::new()?;
    let mut event_loop: EventLoop<LiveNodes> = EventLoop::try_new().map_err(io::Error::other)?;
    let handle = event_loop.handle();
    let (tx, rx) = channel::channel::<NodeEvent>();
    handle
        .insert_source(rx, |ev, _, live: &mut LiveNodes| {
            if let channel::Event::Msg(ev) = ev {
                if let NodeEvent::NodeGone { node } = ev {
                    live.gone(node);
                }
                println!("{ev:?}");
            }
        })
        .map_err(|e| io::Error::other(e.error))?;

    let mut live = LiveNodes::default();
    for n in found {
        list_start_node(n, &mut live, true, &cfg, &tx);
    }
    let hotplug_cfg = cfg.clone();
    handle
        .insert_source(Generic::new(hotplug, Interest::READ, Mode::Level), move |_, hp, live| {
            for n in hp.drain() {
                list_start_node(n, live, false, &hotplug_cfg, &tx);
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| io::Error::other(e.error))?;

    let end = Instant::now() + duration;
    while let Some(left) = end.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        event_loop.dispatch(left, &mut live).map_err(io::Error::other)?;
    }
    Ok(())
}

fn list_start_node(n: FoundNode, live: &mut LiveNodes, initial_enumeration: bool, cfg: &Config, tx: &Sender<NodeEvent>) {
    let Some(id) = live.admit(&n) else { return };
    println!("nó {id}: {} {:?} {:04X}", n.devnode.display(), n.kind, n.product);
    if let Err(e) = spawn_node(n.devnode.clone(), id, n.kind, n.product, initial_enumeration, cfg, tx.clone()) {
        eprintln!("logi-osd: {}: {e}", n.devnode.display());
        live.gone(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_has_one_pill_per_copy_row() {
        let d = Durations { normal: Duration::from_millis(1500), critical: Duration::from_millis(4000) };
        let pills = demo_pills(&d);
        assert_eq!(pills.len(), 10);
        assert_eq!(pills[4].value, message::Value::Dots { on: 3, total: 8 });
        assert_eq!(pills[9].label, "MX Keys Mini conectado");
    }
}
