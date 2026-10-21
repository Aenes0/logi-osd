# logi-osd — Design

> Data: 2026-10-21 · Status: rascunho para aprovação
> Repositório: `github.com/<usuário>/logi-osd`, público, licença GPL-3.0-only (mesma dos outros projetos do usuário: quick-settings, start-menu, taskbar)

## 1. Intenção e o porquê

No Windows, o Logi Options+ mostrava avisos rápidos no **centro inferior da tela** sempre que algo acontecia num dispositivo Logitech: Caps Lock ligado, nível da luz do teclado, bateria baixa, troca de computador pelo Easy-Switch e outros. No Linux (CachyOS + COSMIC 1.10) não existe nada equivalente:

- O **Solaar** (já instalado) só configura os dispositivos. Não mostra nada na tela.
- O **cosmic-osd** mostra volume, brilho e microfone, mas nada específico de teclado ou da Logitech.

O usuário evitou o Logi Options+ porque ele é pesado e tem telemetria. Por isso o logi-osd precisa ser o oposto disso: **extremamente leve e otimizado**, sem rede e sem telemetria.

## 2. Objetivos

O **logi-osd** é um daemon de sessão de usuário que mostra uma pílula de aviso (OSD) no centro inferior da tela para os eventos abaixo, em **todos os dispositivos Logitech** conectados.

### 2.1 Eventos

| Evento | Texto / conteúdo (pt-BR) | Duração |
|---|---|---|
| Caps Lock | `⇪ Caps Lock · Ativado/Desativado` | ~1,5 s |
| Num Lock | `Num Lock · Ativado/Desativado` | ~1,5 s |
| Insert | `Insert · Ativado/Desativado` | ~1,5 s |
| Trava do Fn (Fn+Esc) | ex.: `Fn · Teclas F` / `Fn · Teclas de mídia` | ~1,5 s |
| Luz do teclado (teclas de luz +/−) | ícone de teclado com raios + `Teclado` + nível em **bolinhas** (acesas na cor de destaque, apagadas em cinza). Número de bolinhas = número real de níveis do dispositivo | ~1,5 s |
| Bateria baixa | `<Nome do dispositivo>: bateria baixa · 15%` em **laranja**; em 5%, em **vermelho** | 1,5 s (15%) / ~4 s (5%) |
| Carregando | `<Nome> carregando · 42%` ao conectar o cabo | ~1,5 s |
| Carregado | `<Nome> carregado` ao chegar em 100% | ~1,5 s |
| Conectou | `<Nome> conectado` (com o canal do Easy-Switch, se houver), quando o dispositivo volta para este PC (ligado ou trocado de canal) | ~1,5 s |
| Tecla Emoji | abre o **seletor de emoji** (projeto separado, ver §6) | — |

### 2.2 Regras de comportamento (decididas pelo usuário)

- Bateria baixa avisa **uma vez** ao cruzar 15% e **uma vez** ao cruzar 5%. Não repete enquanto o nível continuar abaixo. O aviso volta a poder disparar depois que o dispositivo for carregado acima do limite.
- **Não avisa** quando o dispositivo desconecta ou dorme, porque isso acontece toda hora em economia de energia.
- Um aviso novo **substitui** o que está na tela. Nunca empilha.
- **Tecla de Ditado: sem ação** por enquanto. Não mostra aviso nem faz nada.
- Sem tela de configuração. Os números (limites de bateria, durações) podem ficar num arquivo de texto simples opcional. Se o arquivo não existir, valem os padrões acima.

## 3. Visual (decidido com mockups)

- **Estilo B, "igual ao OSD do volume do COSMIC"**: pílula escura de cantos totalmente arredondados, ~260 px de largura, borda sutil, ícone à esquerda, texto, e o valor alinhado à direita.
- Cor de destaque e modo claro/escuro **lidos do tema do COSMIC** (`~/.config/cosmic/com.system76.CosmicTheme.*`), para acompanhar o sistema. Laranja e vermelho ficam reservados para bateria.
- Posição: **centro inferior** da tela.
- Luz do teclado: **ícone de teclado com raios** + texto "Teclado" + **bolinhas**. O usuário rejeitou a barra contínua porque ela confunde com o brilho do monitor.
- Mockups de referência: `~/.cache/keyboard-osd-brainstorm/.superpowers/brainstorm/*/content/luz-teclado-v2.html` (opção 3 e a fileira "outros avisos").

## 4. Não-objetivos

- Não substitui o Solaar. Não pareia, não remapeia e não configura dispositivos.
- Não implementa ditado por voz.
- Sem interface de configuração e sem ícone na bandeja.
- Sem suporte a outras marcas.
- Sem rede e sem telemetria de nenhum tipo.

## 5. Anti-objetivos (o que tornaria isto um fracasso, mesmo funcionando)

- **Consumo em repouso perceptível.** Nada de polling: o daemon só acorda quando chega um evento do dispositivo ou do kernel. CPU parada ≈ 0, RAM de poucos MB. O usuário já fez trabalho de otimização de CPU ociosa nos applets dele e vai medir.
- **Atraso visível** entre apertar Caps Lock e a pílula aparecer.
- **Puxar um toolkit gráfico inteiro** (libcosmic/iced/GTK) só para desenhar uma pílula.
- **Exigir privilégios de keylogger**, ou seja, ler todo o teclado via grupo `input`. O acesso deve se limitar ao que já é concedido hoje: `hidraw` dos dispositivos Logitech e `uinput`, ambos por `uaccess` via a regra udev do Solaar (`/usr/lib/udev/rules.d/42-logitech-unify-permissions.rules`).
- **Brigar com o Solaar.** Os dois podem estar rodando ao mesmo tempo, e o logi-osd não pode quebrar as configurações que o Solaar aplica.
- **Alterar como as teclas funcionam hoje**, com exceção da tecla Emoji.

## 6. Relação com o seletor de emoji

O seletor de emoji é **outro projeto e outro repositório**, a ser desenhado depois deste. O logi-osd só precisa **detectar a tecla Emoji e executar um comando configurável** que abre o seletor. Até o seletor existir, a tecla não faz nada.

## 7. Restrições e fatos levantados no sistema

- **Sistema:** CachyOS, kernel 7.2, COSMIC 1.10 (Wayland, cosmic-comp), fcitx5 instalado.
- **Dispositivos atuais:** MX Keys Mini (Bolt, receptor `046d:c548`) e G602 (receptor `046d:c537`). O MX Keys Mini também está pareado com outros hosts (Easy-Switch: 3 canais).
- **Funções HID++ 2.0 do MX Keys Mini** (lidas pelo `solaar show`):
  - `UNIFIED BATTERY {1004}`
  - `BACKLIGHT2 {1982}`
  - `LOCK KEY STATE {4220}`
  - `REPROG CONTROLS V4 {1B04}`: teclas desviáveis, incluindo Emoji, Dictation, Backlight Up/Down e Screen Capture
  - `CHANGE HOST {1814}`
  - `K375S FN INVERSION {40A3}`
  - `KEYBOARD DISABLE KEYS {4521}`: lista Caps Lock, Insert e Win
- **G602:** HID++ mais antigo, com `BATTERY STATUS {1000}`. As respostas dele são instáveis: o Solaar e o OpenRGB registram falhas de descoberta.
- **Permissões:** `/dev/hidraw*` e `/dev/uinput` já são acessíveis ao usuário por ACL (`uaccess`).
- **Projetos irmãos:** Rust, edition 2024, `[profile.release]` com `opt-level=3`, `panic="abort"`, `lto="thin"`. Ambiente de desenvolvimento dentro de um sandbox Flatpak: build e execução via `flatpak-spawn --host`.
- **Desenho:** Wayland layer-shell, que o cosmic-comp suporta, com renderização mínima por software. O usuário aceitou explicitamente **não** usar libcosmic neste projeto por causa de leveza.

## 8. Decisões tomadas por mim (não pelo usuário) — conferir

- Rust + Wayland layer-shell direto (`smithay-client-toolkit`) + rasterização leve (ex.: `tiny-skia`), em vez de libcosmic.
- Fonte de eventos: **HID++ via hidraw**, ouvindo as notificações do próprio dispositivo (lock keys, backlight, bateria, troca de host), em vez de ler `/dev/input`.
- Execução como **serviço systemd de usuário**, iniciado com a sessão gráfica.
- Textos em pt-BR, sem i18n por enquanto.
- Durações e limites de bateria conforme §2. Valores propostos por mim e aceitos pelo usuário.
- **Dispositivos antigos sem notificação de bateria (ex.: G602):** consulta lenta de bateria, **uma leitura a cada 15 min**, só para esses dispositivos. Um timer único e ocioso entre as leituras. É a única exceção à regra de não fazer polling. Os dispositivos que notificam sozinhos nunca são consultados periodicamente. (O usuário respondeu "vai" e deixou essa decisão para mim.)
- **Multi-monitor:** a superfície layer-shell é criada **sem fixar output**, e o cosmic-comp a coloca na tela ativa (a que tem foco). Sem pílula em todas as telas.
- **Luz do teclado:** a pílula aparece **só quando o usuário aperta as teclas de luz +/−**, não quando a luz muda sozinha (sensor de luz ambiente, apagar por ociosidade ou ao tirar as mãos), porque essas mudanças automáticas são constantes e virariam ruído. Se o evento `BACKLIGHT2` não distinguir as duas origens, quem constrói deve relatar antes de escolher entre filtrar eventos e desviar as teclas.
- **Funciona sem o Solaar rodando:** o logi-osd enumera receptores e dispositivos sozinho e convive com o Solaar quando os dois estão abertos.
- **Transportes:** receptores Bolt/Unifying **e** dispositivos conectados direto por Bluetooth. O MX Keys Mini aparece pareado duas vezes com este PC (Host 0 e Host 2 = A-SERVER), então um dos canais provavelmente é Bluetooth direto.
- **Meta de recursos em repouso (critério de aceite):** CPU ≈ 0% (nenhum wakeup periódico, exceto o timer de §8 quando houver dispositivo antigo) e **RSS ≤ ~10 MB**. A meta é ficar abaixo disso; o número exato fica para medir.

## 9. Deixado para quem for construir

- Escolha exata de crates, estrutura de módulos e estratégia de renderização de texto e ícones (fonte do sistema vs. glifos embutidos), desde que respeitem os anti-objetivos de §5.
- Como detectar **Insert** e a **trava do Fn**: via HID++ (`LOCK KEY STATE`, `K375S FN INVERSION`, desvio de tecla com reinjeção via `uinput`) ou outro caminho dentro das permissões de §5. Se Insert não puder ser detectado sem privilégio extra, relatar antes de contornar.
- Como desviar a tecla Emoji sem perder a função das outras teclas e sem conflitar com o Solaar.
- Se a superfície Wayland fica criada e oculta entre avisos (abre mais rápido) ou é criada a cada aviso (menos memória). Decidir medindo.
- Hotplug de receptores e dispositivos (ligar e desligar, trocar de canal).
- Formato e local do arquivo de configuração opcional.
- Testes e como medir CPU/RAM em repouso como critério de aceite.
