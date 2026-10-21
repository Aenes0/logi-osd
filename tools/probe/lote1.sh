#!/bin/sh
# Lote 1 da sonda: cenários curtos com o usuário no teclado (descartável).
# Cada etapa espera Enter antes de começar a gravar.
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

etapa num 15 "Aperte Num Lock 4 vezes (se o teclado nao tiver, so espere)"
etapa insert 15 "Aperte Insert (ou Fn+tecla que produz Insert) 4 vezes"
etapa fn-esc 20 "Aperte Fn+Esc, aperte F1, aperte Fn+Esc de novo, aperte F1"
etapa luz-manual 20 "Aperte luz + 3 vezes e depois luz - 3 vezes"
etapa emoji-normal 15 "Aperte a tecla Emoji 2 vezes e repare o que acontece na tela"
echo
echo "LOTE 1 CONCLUIDO"
