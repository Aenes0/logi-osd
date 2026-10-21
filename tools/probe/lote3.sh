#!/bin/sh
# Lote 3 da sonda: carga (opcional) + teste longo de sono e bateria do G602 (descartável).
set -e
cd "$(dirname "$0")/../.."
P="python3 -I tools/probe/hidpp_probe.py"

echo "############################################################"
echo "TESTE DE CARGA: tem o cabo USB-C do MX Keys Mini?"
printf "Digite s e Enter para fazer, ou so Enter para pular... "
read resp
if [ "$resp" = "s" ]; then
  $P gravar carga 90 "AGORA: conecte o cabo USB-C no MX Keys Mini, espere 30 s, desconecte"
else
  echo ">>> carga pulada"
fi

echo
echo ">>> Ping do G602 (10 tentativas)"
NO=$($P listar 2>/dev/null | awk '/C537/ && /SELECIONADO/ {print $1; exit}')
$P ping "${NO:-c537}" 1 10 > tests/fixtures/probe/ping-g602.txt 2>&1 || true
tail -3 tests/fixtures/probe/ping-g602.txt

echo
echo "############################################################"
echo "TESTE LONGO (20 min): NAO toque no teclado nos primeiros 10 min."
echo "Pode usar o mouse a vontade. Depois de 10 min, aperte uma tecla e use normalmente."
printf "Aperte Enter para comecar... "
read _
$P gravar sono-g602 1200 "AGORA: nao toque no teclado por 10 min; o mouse pode usar"
echo
echo "LOTE 3 CONCLUIDO"
