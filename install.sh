#!/bin/sh
# Instala o logi-osd para o usuário atual (padrão) ou em outro prefixo gravável pelo usuário:
#   ./install.sh                   -> ~/.local
#   ./install.sh /caminho/prefixo  -> prefixo informado (precisa ser gravável pelo usuário)
#   ./install.sh --com-regra-udev  -> instala a regra udev mesmo com a do Solaar presente
#   ./install.sh --remover         -> desfaz a instalação
set -e
cd "$(dirname "$0")"

prefix="$HOME/.local"
force_udev=no
remover=no
for arg in "$@"; do
    case "$arg" in
        --com-regra-udev) force_udev=yes ;;
        --remover) remover=yes ;;
        -*) echo "Opção desconhecida: $arg" >&2; exit 2 ;;
        *) prefix=$(realpath -m "$arg") ;;
    esac
done

unit="$HOME/.config/systemd/user/logi-osd.service"
udev_solaar_usr=/usr/lib/udev/rules.d/42-logitech-unify-permissions.rules
udev_solaar_etc=/etc/udev/rules.d/42-logitech-unify-permissions.rules
udev_ours=/etc/udev/rules.d/70-logi-osd.rules
udev_manual="sudo rm $udev_ours && sudo udevadm control --reload"

if [ "$remover" = yes ]; then
    if systemctl --user disable --now logi-osd.service 2>/dev/null; then
        echo "Serviço parado e desativado."
    else
        echo "Serviço não estava ativo (ou o systemd do usuário não respondeu)."
    fi
    rm -f "$prefix/bin/logi-osd" "$unit" "$prefix/share/logi-osd/config.exemplo"
    rmdir "$prefix/share/logi-osd" 2>/dev/null || true
    systemctl --user daemon-reload 2>/dev/null || true
    echo "Binário e unidade removidos. Sua configuração (~/.config/logi-osd/config) foi mantida."

    # Só remove a regra udev se for idêntica à que este projeto instala.
    if [ -e "$udev_ours" ] && cmp -s data/70-logi-osd.rules "$udev_ours"; then
        echo "Removendo a regra udev do logi-osd (vai pedir sudo)."
        if sudo rm -f "$udev_ours" && sudo udevadm control --reload; then
            echo "Regra udev removida."
        else
            echo "Não foi possível remover a regra udev. Remova manualmente com:" >&2
            echo "  $udev_manual" >&2
        fi
    elif [ -e "$udev_ours" ]; then
        echo "$udev_ours existe mas não é a regra deste projeto; não foi removida."
    fi
    exit 0
fi

cargo build --release
install -Dm0755 target/release/logi-osd "$prefix/bin/logi-osd"
mkdir -p "$(dirname "$unit")"
bindir_esc=$(printf '%s' "$prefix/bin" | sed -e 's/[\\&|]/\\&/g')
sed "s|@bindir@|$bindir_esc|" data/logi-osd.service > "$unit"
install -Dm0644 data/config.exemplo "$prefix/share/logi-osd/config.exemplo"

# Acesso aos dispositivos (/dev/hidraw*): a regra do Solaar dá esse acesso enquanto ele estiver instalado.
solaar_rule_present=no
for rule in "$udev_solaar_usr" "$udev_solaar_etc"; do
    [ -e "$rule" ] && solaar_rule_present=yes
done
if [ "$force_udev" = yes ] || { [ "$solaar_rule_present" = no ] && [ ! -e "$udev_ours" ]; }; then
    cat <<EOF
O logi-osd precisa de acesso aos dispositivos Logitech (/dev/hidraw*).
Esse acesso vinha da regra udev do Solaar, que não está presente (ou será ignorada por --com-regra-udev).
Vou instalar a regra $udev_ours, o que pede sudo.
EOF
    sudo install -Dm0644 data/70-logi-osd.rules "$udev_ours"
    sudo udevadm control --reload
    sudo udevadm trigger --subsystem-match=hidraw
    sudo udevadm settle
fi

systemctl --user daemon-reload
systemctl --user enable logi-osd.service
systemctl --user restart logi-osd.service || echo "Aviso: não foi possível reiniciar o serviço agora (sem sessão gráfica?). Ele inicia no próximo login."

if command -v solaar >/dev/null 2>&1; then
    cat <<EOF
Solaar encontrado. Se você configurar comando_emoji, no Solaar, em "Desvio de teclas",
deixe a tecla Emoji como "Desviado" para os dois não brigarem.
Sem comando_emoji, o logi-osd não desvia a tecla Emoji e não há nada a ajustar.
EOF
fi

echo "Instalado. Para personalizar, copie $prefix/share/logi-osd/config.exemplo para \$XDG_CONFIG_HOME/logi-osd/config (ou ~/.config/logi-osd/config)."
echo "Verifique com: systemctl --user is-active logi-osd"
echo "Os modos de depuração --listar-nos e --demo precisam de $prefix/bin no PATH para rodar o binário pelo nome."
