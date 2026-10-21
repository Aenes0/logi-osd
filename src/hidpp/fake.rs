//! Dublês de `HidIo` para testes: `FakeIo` (roteiro fixo) e `FakeDevice` (responde por regra).
//! Ambos usam relógio virtual: nenhum teste dorme de verdade.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use super::frame::Frame;
use super::transport::HidIo;

/// Quanto o relógio virtual avança a cada frame entregue.
pub const STEP_TICK: Duration = Duration::from_millis(1);

#[derive(Debug, Clone)]
pub enum Step {
    /// Entrega estes bytes no próximo `read` (avança 1 ms).
    Reply(Vec<u8>),
    /// O `read` estoura o prazo pedido (avança o `timeout` inteiro).
    Timeout,
    /// Entrega estes bytes em todo `read`, para sempre (avança 1 ms cada). Não sai do roteiro.
    Forever(Vec<u8>),
    /// O `read` falha com este erro de E/S.
    Fail(io::ErrorKind),
}

/// Relógio virtual: começa num `Instant` fixo e só anda quando o dublê manda.
#[derive(Debug, Clone, Copy)]
struct Clock {
    start: Instant,
    elapsed: Duration,
}

impl Clock {
    fn new() -> Self { Clock { start: Instant::now(), elapsed: Duration::ZERO } }
    fn now(&self) -> Instant { self.start + self.elapsed }
    fn advance(&mut self, d: Duration) { self.elapsed += d; }
}

fn deliver(buf: &mut [u8; 32], bytes: &[u8]) -> usize {
    let n = bytes.len().min(buf.len());
    buf[..n].copy_from_slice(&bytes[..n]);
    n
}

/// Roteiro fixo de leituras. Roteiro vazio = timeout.
pub struct FakeIo {
    pub written: Vec<Vec<u8>>,
    script: VecDeque<Step>,
    clock: Clock,
    /// Erro a devolver no próximo `write` (uma vez).
    pub fail_write: Option<io::ErrorKind>,
}

impl FakeIo {
    pub fn new(script: impl IntoIterator<Item = Step>) -> Self {
        FakeIo { written: Vec::new(), script: script.into_iter().collect(), clock: Clock::new(), fail_write: None }
    }
}

impl HidIo for FakeIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(kind) = self.fail_write.take() {
            return Err(kind.into());
        }
        self.written.push(bytes.to_vec());
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8; 32], timeout: Duration) -> io::Result<Option<usize>> {
        match self.script.front().cloned() {
            Some(Step::Forever(bytes)) => {
                self.clock.advance(STEP_TICK);
                Ok(Some(deliver(buf, &bytes)))
            }
            Some(Step::Reply(bytes)) => {
                self.script.pop_front();
                self.clock.advance(STEP_TICK);
                Ok(Some(deliver(buf, &bytes)))
            }
            Some(Step::Fail(kind)) => {
                self.script.pop_front();
                Err(kind.into())
            }
            Some(Step::Timeout) | None => {
                self.script.pop_front();
                self.clock.advance(timeout);
                Ok(None)
            }
        }
    }

    fn now(&self) -> Instant { self.clock.now() }
}

/// Regra de resposta: recebe a requisição decodificada e devolve os frames crus a entregar, na ordem.
pub type Rule = Box<dyn Fn(&Frame) -> Vec<Vec<u8>>>;

/// Dispositivo simulado: cada `write` passa pela regra e as respostas entram na fila de leitura.
pub struct FakeDevice {
    pub written: Vec<Vec<u8>>,
    queue: VecDeque<Vec<u8>>,
    rule: Rule,
    clock: Clock,
}

impl FakeDevice {
    pub fn new(rule: impl Fn(&Frame) -> Vec<Vec<u8>> + 'static) -> Self {
        FakeDevice { written: Vec::new(), queue: VecDeque::new(), rule: Box::new(rule), clock: Clock::new() }
    }

    /// Enfileira um frame não solicitado (evento/notificação), entregue antes das respostas futuras.
    pub fn push(&mut self, bytes: Vec<u8>) { self.queue.push_back(bytes); }
}

impl HidIo for FakeDevice {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.written.push(bytes.to_vec());
        if let Some(req) = Frame::parse(bytes) {
            self.queue.extend((self.rule)(&req));
        }
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8; 32], timeout: Duration) -> io::Result<Option<usize>> {
        match self.queue.pop_front() {
            Some(bytes) => {
                self.clock.advance(STEP_TICK);
                Ok(Some(deliver(buf, &bytes)))
            }
            None => {
                self.clock.advance(timeout);
                Ok(None)
            }
        }
    }

    fn now(&self) -> Instant { self.clock.now() }
}
