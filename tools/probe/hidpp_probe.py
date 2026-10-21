#!/usr/bin/env python3
"""Sonda HID++ descartável (Tarefa 1 do logi-osd).

Só stdlib. Reproduz o framing do Solaar 1.1.20 (base.py / hidpp20.py / receiver.py /
hidapi/udev_impl.py) sem importá-lo. Rodar no host:

    python3 -I tools/probe/hidpp_probe.py listar
    python3 -I tools/probe/hidpp_probe.py info
    python3 -I tools/probe/hidpp_probe.py gravar <cenario> <segundos> "<prompt>"
    python3 -I tools/probe/hidpp_probe.py desviar <cid_hex> on|off
    python3 -I tools/probe/hidpp_probe.py analytics <cid_hex> on|off
    python3 -I tools/probe/hidpp_probe.py ping <nó> <dev> <n>

Fixture (.hex): uma linha por frame recebido `<t_ms> <nó> <hex ...>`; `# ` = comentário.
"""

import os
import select
import struct
import sys
import time
from pathlib import Path

SW_ID = 0x0C  # o nosso; Solaar usa 0x0B
VENDOR_LOGITECH = 0x046D
SHORT_ID, LONG_ID = 0x10, 0x11
SHORT_LEN, LONG_LEN = 7, 20
TIMEOUT = 1.0           # receptor (dev 0xFF); Solaar usa 0.9 s
TIMEOUT_LONG_REG = 2.0  # leitura longa de registrador (sub 0x83)
# Divergência do brief (1 s): o G602 ocioso responde em ~0.95-1.1 s (medido); Solaar usa 4 s
# (base.py DEFAULT_TIMEOUT) para requests e ping de dispositivo.
TIMEOUT_DEVICE = 4.0
BOLT_PIDS = {0xC548}
FLAG_WIRELESS = 0x000100
FLAG_SOFTWARE_PRESENT = 0x000800

ROOT = Path(__file__).resolve().parents[2]
OUT_DIR = ROOT / "tests" / "fixtures" / "probe"
INFO_PATH = OUT_DIR / "info.txt"

FEATURES = [0x0001, 0x0005, 0x1000, 0x1004, 0x1814, 0x1815, 0x1982, 0x1B04,
            0x1D4B, 0x4220, 0x4521, 0x40A3]
FEATURE_NAMES = {
    0x0000: "ROOT", 0x0001: "FEATURE_SET", 0x0003: "DEVICE_FW_VERSION",
    0x0005: "DEVICE_NAME", 0x0020: "CONFIG_CHANGE", 0x1000: "BATTERY_STATUS",
    0x1004: "UNIFIED_BATTERY", 0x1814: "CHANGE_HOST", 0x1815: "HOSTS_INFO",
    0x1982: "BACKLIGHT2", 0x1B04: "REPROG_CONTROLS_V4", 0x1D4B: "WIRELESS_DEVICE_STATUS",
    0x4220: "LOCK_KEY_STATE", 0x4521: "KEYBOARD_DISABLE_KEYS", 0x40A3: "K375S_FN_INVERSION",
}
CIDS = {0x0103: "Ditado", 0x0108: "Emoji", 0x00E2: "luz-", 0x00E3: "luz+", 0x010A: "captura"}


def hx(data):
    return " ".join(f"{b:02x}" for b in data)


# ----------------------------------------------------------------- descritor HID

def input_report_bits(desc):
    """Bits de entrada por report ID (parser mínimo de report descriptor)."""
    sizes = {}
    rid, rsize, rcount = 0, 0, 0
    stack = []
    i = 0
    while i < len(desc):
        prefix = desc[i]
        if prefix == 0xFE:  # long item
            i += 3 + (desc[i + 1] if i + 1 < len(desc) else 0)
            continue
        n = (0, 1, 2, 4)[prefix & 0x03]
        val = int.from_bytes(desc[i + 1:i + 1 + n], "little")
        typ, tag = (prefix >> 2) & 0x03, prefix >> 4
        if typ == 1:  # global
            if tag == 0x7:
                rsize = val
            elif tag == 0x9:
                rcount = val
            elif tag == 0x8:
                rid = val
            elif tag == 0xA:
                stack.append((rid, rsize, rcount))
            elif tag == 0xB and stack:
                rid, rsize, rcount = stack.pop()
        elif typ == 0 and tag == 0x8:  # main Input
            sizes[rid] = sizes.get(rid, 0) + rsize * rcount
        i += 1 + n
    return sizes


def list_nodes():
    nodes = []
    base = Path("/sys/class/hidraw")
    for entry in sorted(base.glob("hidraw*"), key=lambda p: int(p.name[6:])):
        dev = entry / "device"
        props = {}
        try:
            for line in (dev / "uevent").read_text().splitlines():
                k, _, v = line.partition("=")
                props[k] = v
        except OSError:
            continue
        hid_id = props.get("HID_ID", "")
        try:
            bus, vid, pid = (int(x, 16) for x in hid_id.split(":"))
        except ValueError:
            bus = vid = pid = -1
        try:
            bits = input_report_bits((dev / "report_descriptor").read_bytes())
        except OSError:
            bits = {}
        short = bits.get(SHORT_ID) == 6 * 8
        long_ = bits.get(LONG_ID) == 19 * 8
        driver = props.get("DRIVER", "")
        # filho de receptor: o pai HID também é HID (Solaar: hid_device.find_parent("hid"))
        real = dev.resolve()
        parent_is_hid = (real.parent / "uevent").exists() and "HID_ID=" in (
            (real.parent / "uevent").read_text(errors="replace"))
        child = driver == "logitech-hidpp-device" or parent_is_hid
        if vid != VENDOR_LOGITECH:
            sel, why = False, "vendor != 046d"
        elif not (short or long_):
            sel, why = False, "sem HID++ no descritor"
        elif child:
            sel, why = False, "filho DJ, ignorado"
        else:
            sel, why = True, "SELECIONADO"
        nodes.append({
            "devnode": "/dev/" + entry.name, "hid_id": hid_id, "bus": bus, "vid": vid,
            "pid": pid, "driver": driver, "name": props.get("HID_NAME", ""),
            "short": short, "long": long_, "child": child, "selected": sel, "why": why,
            "tag": f"{pid:04x}" if pid >= 0 else entry.name,
        })
    return nodes


def selected_nodes():
    return [n for n in list_nodes() if n["selected"]]


def find_node(spec):
    spec = spec.lower()
    for n in list_nodes():
        if spec in (n["tag"], n["devnode"], n["devnode"][5:]):
            if n["selected"] or spec != n["tag"]:
                return n
    raise SystemExit(f"nó não encontrado: {spec}")


# ----------------------------------------------------------------- transporte

class Node:
    def __init__(self, info, on_frame=None):
        self.info = info
        self.tag = info["tag"]
        self.fd = os.open(info["devnode"], os.O_RDWR | os.O_NONBLOCK)
        self.t0 = time.monotonic()
        self.frames = []  # (t_ms, bytes) — tudo que chegou e não casou com request
        self.on_frame = on_frame

    def close(self):
        os.close(self.fd)

    def write(self, dev, sub, addr, params=b"", long_=False):
        params = bytes(params)
        if long_ or len(params) > 3:
            frame = bytes([LONG_ID, dev, sub, addr]) + params.ljust(16, b"\0")
        else:
            frame = bytes([SHORT_ID, dev, sub, addr]) + params.ljust(3, b"\0")
        os.write(self.fd, frame)
        return frame

    def read(self, timeout):
        r, _, _ = select.select([self.fd], [], [], max(0.0, timeout))
        if not r:
            return None
        try:
            data = os.read(self.fd, 32)
        except BlockingIOError:
            return None
        return data

    def note(self, data):
        t = int((time.monotonic() - self.t0) * 1000)
        self.frames.append((t, data))
        if self.on_frame:
            self.on_frame(self, t, data)

    def request(self, dev, sub, addr, params=b"", long_=False, timeout=None):
        """Retorna ('ok', params_resposta) | ('err10', code) | ('err20', code) | ('timeout', None)."""
        params = bytes(params)
        if timeout is None:
            if dev == 0xFF:
                timeout = TIMEOUT_LONG_REG if sub == 0x83 else TIMEOUT
            else:
                timeout = TIMEOUT_DEVICE
        self.write(dev, sub, addr, params, long_)
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                return ("timeout", None)
            data = self.read(left)
            if data is None:
                continue
            if len(data) >= SHORT_LEN and data[0] in (SHORT_ID, LONG_ID) and data[1] in (dev, dev ^ 0xFF):
                if data[0] == SHORT_ID and data[2] == 0x8F and data[3] == sub and data[4] == addr:
                    return ("err10", data[5])
                if data[2] == 0xFF and data[3] == sub and data[4] == addr:
                    return ("err20", data[5])
                if data[2] == sub and data[3] == addr:
                    if dev == 0xFF and sub == 0x83 and addr == 0xB5 and params and data[4] != params[0]:
                        self.note(data)
                        continue
                    return ("ok", data[4:])
            self.note(data)

    def feature(self, dev, idx, fn, params=b""):
        return self.request(dev, idx, (fn << 4) | SW_ID, params)

    def ping(self, dev, timeout=TIMEOUT_DEVICE):
        """Retorna versão (float), ou string de erro. self.last_latency = segundos até a resposta."""
        self.write(dev, 0x00, 0x10 | SW_ID, b"\x00\x00\xAA")
        start = time.monotonic()
        self.last_latency = None
        deadline = start + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                return "timeout"
            data = self.read(left)
            if data is None:
                continue
            if len(data) >= SHORT_LEN and data[0] in (SHORT_ID, LONG_ID) and data[1] in (dev, dev ^ 0xFF):
                if data[2] == 0x00 and data[3] == (0x10 | SW_ID) and data[6] == 0xAA:
                    self.last_latency = time.monotonic() - start
                    return data[4] + data[5] / 10.0
                if data[0] == SHORT_ID and data[2] == 0x8F and data[3] == 0x00 and data[4] == (0x10 | SW_ID):
                    self.last_latency = time.monotonic() - start
                    return 1.0 if data[5] == 0x01 else f"erro10 0x{data[5]:02x}"
            self.note(data)

    def drain(self, seconds):
        end = time.monotonic() + seconds
        while (left := end - time.monotonic()) > 0:
            data = self.read(left)
            if data is not None:
                self.note(data)


def fmt_result(res):
    kind, val = res
    if kind == "ok":
        return "ok " + hx(val)
    if kind in ("err10", "err20"):
        return f"{kind} 0x{val:02x}"
    return kind


# ----------------------------------------------------------------- info

class Out:
    def __init__(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        self.f = open(path, "w", encoding="utf-8")

    def __call__(self, line=""):
        print(line, flush=True)
        self.f.write(line + "\n")
        self.f.flush()

    def close(self):
        self.f.close()


def read_name(node, dev, idx):
    res = node.feature(dev, idx, 0)
    if res[0] != "ok":
        return None, fmt_result(res)
    length = res[1][0]
    name = b""
    while len(name) < length:
        r = node.feature(dev, idx, 1, [len(name)])
        if r[0] != "ok":
            break
        chunk = r[1][:length - len(name)]
        if not chunk:
            break
        name += chunk
    return name.decode("ascii", "replace"), f"len={length}"


def codename(node, n):
    bolt = node.info["pid"] in BOLT_PIDS
    p = [0x60 + n, 0x01] if bolt else [0x40 + n - 1]
    res = node.request(0xFF, 0x83, 0xB5, p)
    if res[0] != "ok":
        return None, fmt_result(res)
    b = res[1]
    raw = b[3:3 + min(14, b[2])] if bolt else b[2:2 + b[1]]
    return raw.decode("ascii", "replace"), "ok " + hx(b)


def device_info(node, dev, out):
    tag = node.tag
    out(f"--- dispositivo {tag} dev 0x{dev:02x}")
    ver = node.ping(dev)
    lat = f"{node.last_latency * 1000:.0f} ms" if node.last_latency is not None else "-"
    out(f"ping = {ver} (latencia {lat})")
    if not isinstance(ver, float):
        return
    if ver < 2.0:
        out("protocolo HID++ 1.0: sem features 2.0")
        return
    idx = {}
    for feat in FEATURES:
        res = node.feature(dev, 0, 0, [feat >> 8, feat & 0xFF])
        if res[0] == "ok" and res[1][0] != 0:
            idx[feat] = res[1][0]
            out(f"feature 0x{feat:04x} {FEATURE_NAMES.get(feat, '?')}: indice=0x{res[1][0]:02x} "
                f"tipo=0x{res[1][1]:02x} versao={res[1][2]}")
        else:
            out(f"feature 0x{feat:04x} {FEATURE_NAMES.get(feat, '?')}: ausente ({fmt_result(res)})")

    # mapa completo via FEATURE_SET
    full = {0: 0x0000}
    if 0x0001 in idx:
        res = node.feature(dev, idx[0x0001], 0)
        if res[0] == "ok":
            count = res[1][0]
            out(f"FEATURE_SET count = {count}")
            for i in range(1, count + 1):
                r = node.feature(dev, idx[0x0001], 1, [i])
                if r[0] == "ok":
                    fid = (r[1][0] << 8) | r[1][1]
                    full[i] = fid
                    out(f"  [0x{i:02x}] 0x{fid:04x} {FEATURE_NAMES.get(fid, '')} tipo=0x{r[1][2]:02x}")
    else:
        full.update({v: k for k, v in idx.items()})
    for i, fid in sorted(full.items()):
        out(f"@mapa {tag} {dev:02x} {i:02x} {fid:04x}")

    if 0x0005 in idx:
        name, extra = read_name(node, dev, idx[0x0005])
        out(f"nome_0005 = {name} ({extra})")
    if dev != 0xFF:
        cn, extra = codename(node, dev)
        out(f"codinome = {cn} ({extra})")

    if 0x1982 in idx:
        res = node.feature(dev, idx[0x1982], 0)
        out(f"BACKLIGHT2 fn0 = {fmt_result(res)}")
        if res[0] == "ok":
            en, opt, sup, eff, lvl, dho, dhi, dpow = struct.unpack("<BBBHBHHH", bytes(res[1][:12]))
            out(f"  enabled={en} options=0x{opt:02x} supported=0x{sup:02x} effects=0x{eff:04x} "
                f"level={lvl} dho={dho} dhi={dhi} dpow={dpow}")
        res = node.feature(dev, idx[0x1982], 2)
        out(f"BACKLIGHT2 fn2 = {fmt_result(res)}")
        if res[0] == "ok":
            out(f"  backlight_niveis = {res[1][0]}")

    change_host_p1 = None
    if 0x1814 in idx:
        res = node.feature(dev, idx[0x1814], 0)
        out(f"CHANGE_HOST fn0 = {fmt_result(res)}")
        if res[0] == "ok":
            change_host_p1 = res[1][1]
            out(f"  hosts={res[1][0]} atual={res[1][1]}")
    prefix = None
    if 0x1815 in idx:
        res = node.feature(dev, idx[0x1815], 0)
        out(f"HOSTS_INFO fn0 = {fmt_result(res)}")
        if res[0] == "ok":
            prefix = res[1][3]
    p1s = f"0x{change_host_p1:02x}" if change_host_p1 is not None else "n/a"
    pfs = f"0x{prefix:02x}" if prefix is not None else "n/a"
    out(f"hosts_info_byte3 = {pfs}   change_host_p1 = {p1s}")

    if 0x4220 in idx:
        out(f"LOCK_KEY_STATE fn0 = {fmt_result(node.feature(dev, idx[0x4220], 0))}")
    if 0x40A3 in idx:
        pre = prefix if prefix is not None else 0xFF
        res = node.feature(dev, idx[0x40A3], 0, [pre])
        out(f"40A3 fn0 [0x{pre:02x}] = {fmt_result(res)}")
        if res[0] == "ok":
            out(f"  fn_inv_estado (byte 1) = {res[1][1]}")
    if 0x1004 in idx:
        res = node.feature(dev, idx[0x1004], 1)
        out(f"UNIFIED_BATTERY fn1 = {fmt_result(res)}")
        if res[0] == "ok":
            out(f"  soc={res[1][0]} nivel=0x{res[1][1]:02x} status={res[1][2]} ext=0x{res[1][3]:02x}")
    if 0x1000 in idx:
        res = node.feature(dev, idx[0x1000], 0)
        out(f"BATTERY_STATUS fn0 = {fmt_result(res)}")
        if res[0] == "ok":
            out(f"  nivel={res[1][0]} proximo={res[1][1]} status={res[1][2]}")

    if 0x1B04 in idx:
        ri = idx[0x1B04]
        res = node.feature(dev, ri, 0)
        out(f"REPROG_V4 fn0 = {fmt_result(res)}")
        if res[0] == "ok":
            count = res[1][0]
            out(f"  controles = {count}")
            for i in range(count):
                r = node.feature(dev, ri, 1, [i])
                if r[0] != "ok":
                    out(f"  [{i}] {fmt_result(r)}")
                    continue
                cid, tid, f1, pos, grp, gmask, f2 = struct.unpack("!HHBBBBB", bytes(r[1][:9]))
                if cid not in CIDS:
                    continue
                flags = f1 | (f2 << 8)
                out(f"  CID 0x{cid:04x} ({CIDS[cid]}) idx={i} tid=0x{tid:04x} flags=0x{flags:04x} "
                    f"pos={pos} group={grp} gmask=0x{gmask:02x} analytics={'sim' if flags & 0x400 else 'nao'}")
                rep = node.feature(dev, ri, 2, [cid >> 8, cid & 0xFF])
                out(f"    getCidReporting = {fmt_result(rep)}")
                out(f"@cid {tag} {dev:02x} {cid:04x} {flags:04x}")


def receiver_info(node, out):
    out(f"=== receptor {node.tag} ({node.info['devnode']}, {node.info['name']})")
    res = node.request(0xFF, 0x81, 0x00)
    out(f"receptor_flags_antes_{node.tag} = {fmt_result(res)}")
    if res[0] == "ok":
        before = int.from_bytes(res[1][:3], "big")
        out(f"  WIRELESS={'sim' if before & FLAG_WIRELESS else 'nao'} "
            f"SOFTWARE_PRESENT={'sim' if before & FLAG_SOFTWARE_PRESENT else 'nao'}")
        if not before & FLAG_WIRELESS:
            new = before | FLAG_WIRELESS
            w = node.request(0xFF, 0x80, 0x00, new.to_bytes(3, "big"))
            out(f"escrita flags (OR WIRELESS) 0x{new:06x} = {fmt_result(w)}")
            after = node.request(0xFF, 0x81, 0x00)
            out(f"receptor_flags_depois_{node.tag} = {fmt_result(after)}")
        else:
            out(f"receptor_flags_depois_{node.tag} = (sem escrita, igual)")
    mark = len(node.frames)
    trig = node.request(0xFF, 0x80, 0x02, [0x02, 0x00, 0x00])
    out(f"gatilho reg 0x02 = {fmt_result(trig)}")
    node.drain(2.0)
    online = []
    seen = set()
    for t, data in node.frames[mark:]:
        if len(data) >= SHORT_LEN and data[0] == SHORT_ID and data[2] == 0x41:
            d, proto, p0, p1, p2 = data[1], data[3], data[4], data[5], data[6]
            link = not (p0 & 0x40)
            out(f"0x41 dev={d} protocolo=0x{proto:02x} p0=0x{p0:02x} wpid=0x{(p2 << 8) | p1:04x} "
                f"link={'sim' if link else 'nao'} [{hx(data)}]")
            if link and d not in seen:
                online.append(d)
            seen.add(d)
    if not seen:
        out("nenhuma 0x41 em 2 s; fallback: ping 1..6")
        for d in range(1, 7):
            v = node.ping(d)
            out(f"  ping dev {d} = {v}")
            if isinstance(v, float):
                online.append(d)
    for d in online:
        device_info(node, d, out)


def cmd_info(_args):
    out = Out(INFO_PATH)
    out(f"# info gerado em {time.strftime('%Y-%m-%d %H:%M:%S')}")
    for n in selected_nodes():
        node = Node(n)
        try:
            if "receiver" in n["name"].lower():
                receiver_info(node, out)
            else:
                out(f"=== direto {node.tag} ({n['devnode']}, {n['name']})")
                device_info(node, 0xFF, out)
            node.drain(0.3)
            out(f"notificacoes nao casadas em {node.tag}: {len(node.frames)}")
            for t, data in node.frames:
                out(f"  {t:6d} {hx(data)}")
        finally:
            node.close()
        out()
    out.close()


def cmd_listar(_args):
    for n in list_nodes():
        out = (f"{n['devnode']:<14} HID_ID={n['hid_id']:<24} DRIVER={n['driver']:<22} "
               f"HID++={'curto' if n['short'] else '-'}/{'longo' if n['long'] else '-'} "
               f"-> {n['why']}  ({n['name']})")
        print(out)


# ----------------------------------------------------------------- gravar

def load_map():
    lines = []
    if INFO_PATH.exists():
        for line in INFO_PATH.read_text(encoding="utf-8").splitlines():
            if line.startswith("@mapa "):
                _, tag, dev, i, fid = line.split()
                lines.append(f"# feature 0x{i} = 0x{fid} (dev {int(dev, 16)}, {tag})")
    return lines


def cmd_gravar(args):
    if len(args) < 3:
        raise SystemExit('uso: gravar <cenario> <segundos> "<prompt>"')
    cenario, secs, prompt = args[0], float(args[1]), " ".join(args[2:])
    path = OUT_DIR / f"{cenario}.hex"
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    tty = sys.stdout.isatty()
    f = open(path, "w", encoding="utf-8")
    f.write(f"# cenario: {cenario}\n# prompt: {prompt}\n# duracao_s: {secs:g}\n")
    f.write(f"# inicio: {time.strftime('%Y-%m-%d %H:%M:%S')}\n")
    for line in load_map():
        f.write(line + "\n")
    nodes = []
    for n in selected_nodes():
        f.write(f"# no {n['tag']} = {n['devnode']} {n['hid_id']} {n['driver']}\n")
        nodes.append(Node(n))
    f.flush()
    t0 = time.monotonic()
    count = 0
    print("\n" + "=" * 60)
    print(f">>> {prompt}")
    print(f">>> gravando '{cenario}' por {secs:g} s em {path}  (Ctrl-C encerra e salva)")
    print("=" * 60, flush=True)
    fds = {nd.fd: nd for nd in nodes}
    last_status = -1
    try:
        while (el := time.monotonic() - t0) < secs:
            r, _, _ = select.select(list(fds), [], [], min(0.5, secs - el))
            for fd in r:
                try:
                    data = os.read(fd, 32)
                except BlockingIOError:
                    continue
                t = int((time.monotonic() - t0) * 1000)
                nd = fds[fd]
                if len(data) in (SHORT_LEN, LONG_LEN) and data[0] in (SHORT_ID, LONG_ID):
                    line = f"{t:6d} {nd.tag} {hx(data)}"
                    if data[2] == 0x00 and data[3] & 0x0F == 0:
                        line += "\n# (notificacao nula, no-op)"
                    count += 1
                else:
                    line = f"# ignorado (len {len(data)}): {t} {nd.tag} {hx(data)}"
                f.write(line + "\n")
                f.flush()
                if tty:
                    print("\r" + " " * 40 + "\r", end="")
                print(line.split("\n")[0], flush=True)
            sec = int(time.monotonic() - t0)
            if sec != last_status:
                last_status = sec
                restante = max(0, int(secs) - sec)
                if tty:
                    print(f"\r  {sec:4d} s decorridos, {restante:4d} s restantes, {count} frames ",
                          end="", flush=True)
                elif sec % 30 == 0:
                    print(f"  {sec} s decorridos, {restante} s restantes, {count} frames", flush=True)
    except KeyboardInterrupt:
        f.write(f"# interrompido por Ctrl-C em {int((time.monotonic() - t0) * 1000)} ms\n")
        print("\n(interrompido; salvando)")
    finally:
        f.write(f"# fim: {count} frames\n")
        f.close()
        for nd in nodes:
            nd.close()
    print(f"\n>>> fim de '{cenario}': {count} frames em {path}", flush=True)


# ----------------------------------------------------------------- desviar / analytics / ping

def load_cids():
    cids, maps = {}, {}
    if not INFO_PATH.exists():
        raise SystemExit("rode `info` antes")
    for line in INFO_PATH.read_text(encoding="utf-8").splitlines():
        if line.startswith("@cid "):
            _, tag, dev, cid, flags = line.split()
            cids.setdefault(int(cid, 16), []).append((tag, int(dev, 16), int(flags, 16)))
        elif line.startswith("@mapa "):
            _, tag, dev, i, fid = line.split()
            maps[(tag, int(dev, 16), int(fid, 16))] = int(i, 16)
    return cids, maps


def set_reporting(args, analytics):
    if len(args) != 2 or args[1] not in ("on", "off"):
        raise SystemExit(f"uso: {'analytics' if analytics else 'desviar'} <cid_hex> on|off")
    cid = int(args[0], 16)
    on = args[1] == "on"
    cids, maps = load_cids()
    if cid not in cids:
        raise SystemExit(f"CID 0x{cid:04x} não está no info.txt")
    nodes = {n["tag"]: n for n in selected_nodes()}
    for tag, dev, flags in cids[cid]:
        if analytics and not flags & 0x400:
            print(f"CID 0x{cid:04x} em {tag}/{dev}: flags=0x{flags:04x} sem 0x400 -> analytics nao suportado")
            continue
        ri = maps[(tag, dev, 0x1B04)]
        node = Node(nodes[tag])
        try:
            if analytics:
                params = [cid >> 8, cid & 0xFF, 0x00, 0x00, 0x00, 0x03 if on else 0x02]
            else:
                params = [cid >> 8, cid & 0xFF, 0x03 if on else 0x02, 0x00, 0x00]
            res = node.feature(dev, ri, 3, params)
            print(f"setCidReporting {tag}/{dev} [{hx(params)}] = {fmt_result(res)}")
            rep = node.feature(dev, ri, 2, [cid >> 8, cid & 0xFF])
            print(f"getCidReporting = {fmt_result(rep)}")
        finally:
            node.close()


def cmd_desviar(args):
    set_reporting(args, analytics=False)


def cmd_analytics(args):
    set_reporting(args, analytics=True)


def cmd_ping(args):
    if len(args) != 3:
        raise SystemExit("uso: ping <nó> <dev> <n>")
    node = Node(find_node(args[0]))
    dev, n = int(args[1], 0), int(args[2])
    ok = ok_1s = 0
    try:
        for i in range(n):
            v = node.ping(dev)
            lat = node.last_latency
            lats = f"{lat * 1000:.0f} ms" if lat is not None else "-"
            print(f"ping {i + 1}: {v} (latencia {lats})", flush=True)
            if isinstance(v, float):
                ok += 1
                if lat <= TIMEOUT:
                    ok_1s += 1
            node.drain(0.2)
    finally:
        node.close()
    print(f"{ok_1s}/{n} em ate {TIMEOUT:g} s (brief); {ok}/{n} em ate {TIMEOUT_DEVICE:g} s (Solaar)")


COMMANDS = {"listar": cmd_listar, "info": cmd_info, "gravar": cmd_gravar,
            "desviar": cmd_desviar, "analytics": cmd_analytics, "ping": cmd_ping}


def main(argv):
    if len(argv) < 2 or argv[1] not in COMMANDS:
        raise SystemExit("subcomandos: " + ", ".join(COMMANDS))
    COMMANDS[argv[1]](argv[2:])


if __name__ == "__main__":
    main(sys.argv)
