//! Superfície layer-shell da pílula (sctk 0.20, sem seat/teclado/ponteiro).
//!
//! Orçamento: a layer surface, o `SlotPool` (memória do wl_shm) e o buffer só existem enquanto
//! uma pílula está visível. `present` cria; `hide` destrói tudo e a memória volta. Ociosa, a
//! conexão Wayland não tem nenhum objeto da pílula nem timer.
//!
//! Escala: inteira, do evento `preferred_buffer_scale` da superfície (ou, em compositores
//! antigos, da maior escala das saídas em que ela entrou — o sctk deriva isso e chama
//! `scale_factor_changed`). A última escala conhecida vale para a próxima pílula; antes de
//! qualquer superfície usa a maior escala entre as saídas, ou 1.

use std::time::Instant;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::LoopHandle;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm, registry_handlers,
};
use tiny_skia::Pixmap;

use crate::State;
use crate::osd::render::{MARGIN_BOTTOM, to_argb8888};

const NAMESPACE: &str = "logi-osd";

pub struct Osd {
    registry: RegistryState,
    compositor: CompositorState,
    outputs: OutputState,
    shm: Shm,
    layer_shell: LayerShell,
    qh: QueueHandle<State>,
    shown: Option<Shown>,
    /// Última escala preferida pela superfície; 0 = nenhuma ainda.
    scale: u32,
    /// `LOGI_OSD_DEBUG=1`: imprime `t_ms evento→commit`.
    debug: bool,
}

/// Recursos que só existem com uma pílula visível. Ordem de queda: superfície, buffer, pool.
struct Shown {
    layer: LayerSurface,
    buffer: Option<Buffer>,
    pool: Option<SlotPool>,
    /// Primeiro `configure` já veio: pode anexar buffer.
    configured: bool,
    /// Desenho à espera do primeiro `configure` (com a escala em que foi feito).
    pending: Option<(Pixmap, u32)>,
    /// Tamanho lógico pedido com `set_size`.
    size: (u32, u32),
    /// Escala do buffer anexado (ou pendente).
    scale: u32,
    /// Instante do evento que pediu este desenho (só com `debug`).
    since: Option<Instant>,
}

impl Osd {
    /// Conecta ao compositor, liga os globais e põe a fila Wayland no calloop.
    pub fn connect(handle: &LoopHandle<'static, State>) -> Result<Osd, Box<dyn std::error::Error>> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh)?;
        let layer_shell = LayerShell::bind(&globals, &qh)?;
        let shm = Shm::bind(&globals, &qh)?;
        let osd = Osd {
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
            compositor,
            shm,
            layer_shell,
            qh,
            shown: None,
            scale: 0,
            debug: std::env::var_os("LOGI_OSD_DEBUG").is_some_and(|v| v == "1"),
        };
        WaylandSource::new(conn, queue).insert(handle.clone()).map_err(|e| e.error)?;
        Ok(osd)
    }

    /// Mostra (ou troca) a pílula. `pixmap_for_scale(escala)` desenha em pixels de buffer;
    /// a largura/altura lógicas são as do pixmap divididas pela escala. `since` = instante do
    /// evento (para a medição de latência do `LOGI_OSD_DEBUG`).
    pub fn present(&mut self, since: Instant, pixmap_for_scale: impl FnOnce(u32) -> Pixmap) {
        let scale = self.current_scale();
        let pixmap = pixmap_for_scale(scale);
        let size = (pixmap.width() / scale, pixmap.height() / scale);
        let since = self.debug.then_some(since);
        if self.shown.is_none() {
            self.shown = Some(self.create_surface(size, since));
        }
        let Some(shown) = self.shown.as_mut() else { return };
        shown.since = since.or(shown.since);
        if shown.configured {
            draw(shown, &self.shm, pixmap, scale, size, self.debug);
        } else {
            if shown.size != size {
                // Ainda sem configure: só atualiza o pedido; o desenho sai no configure.
                shown.layer.set_size(size.0, size.1);
                shown.size = size;
                shown.layer.commit();
            }
            shown.pending = Some((pixmap, scale));
        }
    }

    /// Destrói a superfície, o buffer e o pool (memória do wl_shm volta ao sistema).
    pub fn hide(&mut self) {
        self.shown = None;
    }

    /// Escala do próximo desenho: a preferida da superfície; senão a maior das saídas; senão 1.
    fn current_scale(&self) -> u32 {
        if self.scale > 0 {
            return self.scale;
        }
        self.outputs
            .outputs()
            .filter_map(|o| self.outputs.info(&o))
            .map(|i| u32::try_from(i.scale_factor).unwrap_or(1))
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// Layer surface: Overlay, centro inferior, sem saída fixa (o compositor escolhe a ativa),
    /// sem teclado, região de entrada vazia, sem zona exclusiva. Commit sem buffer; o desenho
    /// sai no primeiro `configure`.
    fn create_surface(&self, size: (u32, u32), since: Option<Instant>) -> Shown {
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some(NAMESPACE), None);
        layer.set_anchor(Anchor::BOTTOM);
        layer.set_size(size.0, size.1);
        layer.set_margin(0, 0, MARGIN_BOTTOM, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_exclusive_zone(0);
        match Region::new(&self.compositor) {
            // A região é copiada no `set_input_region`; destruí-la logo depois é permitido.
            Ok(empty) => layer.set_input_region(Some(empty.wl_region())),
            Err(e) => eprintln!("logi-osd: região de entrada vazia não criada (a pílula pode capturar cliques): {e}"),
        }
        layer.commit();
        Shown { layer, buffer: None, pool: None, configured: false, pending: None, size, scale: 0, since }
    }

    fn is_ours(&self, surface: &wl_surface::WlSurface) -> bool {
        self.shown.as_ref().is_some_and(|s| s.layer.wl_surface() == surface)
    }
}

/// Anexa `pixmap` (RGBA pré-multiplicado) num buffer Argb8888 do pool e faz commit.
fn draw(shown: &mut Shown, shm: &Shm, pixmap: Pixmap, scale: u32, size: (u32, u32), debug: bool) {
    let (Ok(w), Ok(h)) = (i32::try_from(pixmap.width()), i32::try_from(pixmap.height())) else { return };
    let stride = w * 4;
    let len = pixmap.data().len();
    let pool = match &mut shown.pool {
        Some(pool) => pool,
        None => match SlotPool::new(len, shm) {
            Ok(pool) => shown.pool.insert(pool),
            Err(e) => {
                eprintln!("logi-osd: pool wl_shm não criado: {e}");
                return;
            }
        },
    };
    let (buffer, canvas) = match pool.create_buffer(w, h, stride, wl_shm::Format::Argb8888) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("logi-osd: buffer wl_shm não criado: {e}");
            return;
        }
    };
    to_argb8888(pixmap.data(), canvas);
    drop(pixmap);

    let layer = &shown.layer;
    if shown.size != size {
        layer.set_size(size.0, size.1);
        shown.size = size;
    }
    if layer.set_buffer_scale(scale).is_err() && scale != 1 {
        eprintln!("logi-osd: wl_surface < v3 não aceita escala {scale}");
    }
    layer.wl_surface().damage_buffer(0, 0, w, h);
    if let Err(e) = buffer.attach_to(layer.wl_surface()) {
        eprintln!("logi-osd: buffer não anexado: {e}");
        return;
    }
    layer.commit();
    shown.scale = scale;
    // O buffer anterior cai aqui: se o compositor ainda o usa, o sctk o destrói no release.
    shown.buffer = Some(buffer);
    if debug && let Some(t) = shown.since.take() {
        eprintln!("logi-osd: {:.1} ms evento→commit", t.elapsed().as_secs_f64() * 1000.0);
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, factor: i32) {
        let factor = u32::try_from(factor).unwrap_or(1).max(1);
        self.osd.scale = factor;
        // Compara com a escala do desenho pendente (antes do configure) ou do anexado: igual =
        // nada a redesenhar.
        let stale = self.osd.is_ours(surface)
            && self.osd.shown.as_ref().is_some_and(|s| s.pending.as_ref().map_or(s.scale, |(_, p)| *p) != factor);
        if stale {
            self.redraw();
        }
    }

    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}

    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.osd.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for State {
    /// O compositor fechou a superfície (ex.: saída removida): solta os recursos; a próxima
    /// pílula cria outra.
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.osd.shown.as_ref().is_some_and(|s| &s.layer == layer) {
            self.osd.hide();
        }
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        let debug = self.osd.debug;
        let Some(shown) = self.osd.shown.as_mut().filter(|s| &s.layer == layer) else { return };
        // O tamanho é nosso (ancorado só embaixo): o configure só libera o primeiro desenho.
        shown.configured = true;
        if let Some((pixmap, scale)) = shown.pending.take() {
            let size = shown.size;
            draw(shown, &self.osd.shm, pixmap, scale, size, debug);
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.osd.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.osd.registry
    }

    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_layer!(State);
delegate_registry!(State);
