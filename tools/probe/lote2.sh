#!/bin/sh
# Lote 2 da sonda: luz automática, Easy-Switch/Bluetooth, liga-desliga, Emoji desviado (descartável).
set -e
cd "$(dirname "$0")/../.."
P="python3 -I tools/probe/hidpp_probe.py"

etapa() {
  echo
  echo "############################################################"
  echo "PROXIMA ETAPA: $3"
  printf "Aperte Enter quando estiver pronto... "
  read _
  $P gravar "$1" "$2" "AGORA: $3"
}

copiar_bt() {
  mkdir -p tests/fixtures/descriptors
  achou=0
  for h in /sys/class/hidraw/*; do
    id=$(grep '^HID_ID=' "$h/device/uevent" | cut -d= -f2)
    case "$id" in
      0005:0000046D:*)
        cp "$h/device/report_descriptor" tests/fixtures/descriptors/bt_hidpp.bin
        echo "$(basename "$h") $id $(basename "$(readlink -f "$h/device/driver")")" > tests/fixtures/probe/bt_no.txt
        echo ">>> No Bluetooth Logitech encontrado: $(cat tests/fixtures/probe/bt_no.txt)"
        achou=1 ;;
    esac
  done
  [ "$achou" = 1 ] || { echo ">>> Nenhum no Bluetooth Logitech neste canal"; echo "nenhum" > tests/fixtures/probe/bt_no.txt; }
}

etapa luz-auto 90 "NAO toque no teclado por 45 s; depois aproxime as maos sem apertar nada; por fim cubra e descubra o teclado com a mao"
etapa easyswitch 60 "Troque o teclado para o canal 2 (Fn+F2 ou tecla 2), espere 15 s, volte para o canal 1"

echo
echo "############################################################"
echo "PROXIMA ETAPA (Bluetooth): troque o teclado para o canal 2, espere 5 s e aperte Enter"
read _
$P listar > tests/fixtures/probe/listar-canal2.txt 2>&1 || true
copiar_bt
echo "Agora volte o teclado para o canal 1 e aperte Enter"
read _

etapa desliga-liga 40 "Desligue o MX Keys Mini no interruptor, espere 10 s, ligue de novo"

echo
echo ">>> Desviando a tecla Emoji para teste (sera restaurada no fim)"
$P desviar 0108 on
etapa emoji-desviado 40 "Aperte Emoji 2 vezes; troque para o canal 2 e volte ao 1; aperte Emoji mais 2 vezes"
$P desviar 0108 off
echo ">>> Tecla Emoji restaurada"

echo
echo "LOTE 2 CONCLUIDO"
