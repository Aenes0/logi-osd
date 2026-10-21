mod app;
mod battery;
mod config;
mod devices;
mod emoji;
mod hidpp;
mod message;
mod osd;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::calloop::channel::{self, Sender};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken};

use crate::app::{Action, App, NodeEvent};
use crate::battery::BatteryAlert;
use crate::config::Config;
use crate::devices::node::{FoundNode, Hotplug, LiveNodes, is_new_device, reinspect, spawn_node};
use crate::message::{Durations, LockKey, Pill};
use crate::osd::controller::{OsdAction, OsdController};
use crate::osd::render::Renderer;
use crate::osd::theme;
use crate::osd::wayland::Osd;

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

/// Timer de esconder, armado só com pílula visível.
struct HideTimer { token: RegistrationToken, armed: Instant }

pub struct State {
    app: App,
    osd: Osd,
    controller: OsdController,
    renderer: Renderer,
    cfg: Config,
    /// `~/.config/cosmic` (tema relido a cada pílula).
    cosmic_dir: PathBuf,
    hide_timer: Option<HideTimer>,
    /// Prazo da pílula visível (o timer pode estar armado para antes e então se reprograma).
    deadline: Instant,
    handle: LoopHandle<'static, State>,
    live: LiveNodes,
    tx: Sender<NodeEvent>,
}

/// Repouso: nenhum timer armado e nenhuma superfície; o loop dorme em `run(None)` até um evento
/// de nó, de hotplug ou do compositor.
fn run(mode: RunMode) -> Result<(), BoxError> {
    let cfg = config::load();
    let renderer = Renderer::load(cfg.font.as_deref())?;
    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    let osd = Osd::connect(&handle)?;

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
        osd,
        controller: OsdController::default(),
        renderer,
        cosmic_dir: config::config_home().join("cosmic"),
        cfg,
        hide_timer: None,
        deadline: Instant::now(),
        handle: handle.clone(),
        live: LiveNodes::default(),
        tx,
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
            let pills = demo_pills(&Durations { normal: state.cfg.duration, critical: state.cfg.duration_critical });
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

/// O timer só é recriado se o novo prazo vem antes do armado; senão ele dispara, vê o prazo
/// ainda futuro e se reprograma (sem fonte nova no loop).
fn needs_rearm(armed: Option<Instant>, deadline: Instant) -> bool {
    armed.is_none_or(|armed| deadline < armed)
}

/// O que o timer de esconder faz ao disparar.
#[derive(Debug, PartialEq)]
enum HideDecision {
    /// Prazo atingido: esconde e larga o timer.
    Hide,
    /// Pílula ainda visível com prazo mais tarde (foi trocada): reprograma para o prazo atual.
    Reschedule(Instant),
    /// Nada visível: larga o timer.
    Drop,
}

/// `expired` = resultado de `OsdController::expire` no disparo; `deadline` = prazo da pílula atual.
fn hide_decision(expired: &OsdAction, visible: bool, deadline: Instant) -> HideDecision {
    match expired {
        OsdAction::Hide => HideDecision::Hide,
        _ if visible => HideDecision::Reschedule(deadline),
        _ => HideDecision::Drop,
    }
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

    fn show(&mut self, pill: Pill, now: Instant) {
        let OsdAction::Present { deadline, .. } = self.controller.show(pill, now) else { return };
        self.deadline = deadline;
        self.redraw_at(now);
        self.arm_hide(deadline);
    }

    /// Redesenha a pílula visível (troca de escala da superfície).
    pub(crate) fn redraw(&mut self) {
        self.redraw_at(Instant::now());
    }

    /// Tema relido a cada desenho (só acontece com pílula sendo mostrada).
    fn redraw_at(&mut self, since: Instant) {
        let Some(pill) = self.controller.visible() else { return };
        let theme = theme::load(&self.cosmic_dir);
        let renderer = &self.renderer;
        self.osd.present(since, |scale| renderer.render(pill, &theme, scale));
    }

    fn arm_hide(&mut self, deadline: Instant) {
        if !needs_rearm(self.hide_timer.as_ref().map(|t| t.armed), deadline) {
            return;
        }
        if let Some(old) = self.hide_timer.take() {
            self.handle.remove(old.token);
        }
        match self.handle.insert_source(Timer::from_deadline(deadline), |_, _, s: &mut State| s.on_hide_timer()) {
            Ok(token) => self.hide_timer = Some(HideTimer { token, armed: deadline }),
            Err(e) => {
                // Sem timer a pílula ficaria na tela para sempre: esconde já.
                eprintln!("logi-osd: timer de esconder não criado: {}", e.error);
                self.controller.expire(deadline);
                self.osd.hide();
            }
        }
    }

    fn on_hide_timer(&mut self) -> TimeoutAction {
        let expired = self.controller.expire(Instant::now());
        match hide_decision(&expired, self.controller.visible().is_some(), self.deadline) {
            HideDecision::Hide => {
                self.osd.hide();
                self.hide_timer = None;
                TimeoutAction::Drop
            }
            HideDecision::Reschedule(at) => {
                if let Some(t) = self.hide_timer.as_mut() {
                    t.armed = at;
                }
                TimeoutAction::ToInstant(at)
            }
            HideDecision::Drop => {
                self.hide_timer = None;
                TimeoutAction::Drop
            }
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
    fn hide_timer_rearms_only_for_earlier_deadline() {
        let t = Instant::now();
        assert!(needs_rearm(None, t), "sem timer: arma");
        assert!(needs_rearm(Some(t + Duration::from_secs(4)), t + Duration::from_millis(1500)), "prazo mais cedo: recria");
        assert!(!needs_rearm(Some(t), t + Duration::from_secs(1)), "prazo mais tarde: o timer se reprograma");
        assert!(!needs_rearm(Some(t), t), "mesmo prazo: nada");
    }

    #[test]
    fn hide_timer_hides_when_deadline_reached() {
        let t = Instant::now();
        assert_eq!(hide_decision(&OsdAction::Hide, false, t), HideDecision::Hide);
    }

    #[test]
    fn hide_timer_reschedules_while_pill_still_due_later() {
        // Pílula trocada por outra de prazo mais longo: o timer antigo dispara cedo e precisa
        // se reprogramar; largá-lo deixaria a pílula na tela para sempre.
        let later = Instant::now() + Duration::from_secs(2);
        assert_eq!(hide_decision(&OsdAction::Nothing, true, later), HideDecision::Reschedule(later));
    }

    #[test]
    fn hide_timer_drops_when_nothing_visible() {
        assert_eq!(hide_decision(&OsdAction::Nothing, false, Instant::now()), HideDecision::Drop);
    }

    #[test]
    fn demo_has_one_pill_per_copy_row() {
        let d = Durations { normal: Duration::from_millis(1500), critical: Duration::from_millis(4000) };
        let pills = demo_pills(&d);
        assert_eq!(pills.len(), 10);
        assert_eq!(pills[4].value, message::Value::Dots { on: 3, total: 8 });
        assert_eq!(pills[9].label, "MX Keys Mini conectado");
    }
}
