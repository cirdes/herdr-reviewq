#!/usr/bin/env bash
# Passo `[[build]]` do herdr: baixa da release o binário deste Mac e o instala em ~/.local/bin,
# o caminho que o painel (bin/herdr-reviewq) e o daemon usam. `plugin link` pula este passo.
set -euo pipefail

NAME="herdr-reviewq"
SAY="reviewq"
REPO="cirdes/herdr-reviewq"
LABEL="dev.cirdes.herdr-reviewq"

# A raiz vem do lugar do script: o build pode rodar sem o ambiente do herdr.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# A tag da release é a versão do manifesto, então cada checkout baixa a própria release.
VERSION="$(grep -m1 '^version' "$ROOT/herdr-plugin.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
TAG="v${VERSION}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  target="aarch64-apple-darwin" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  *)
    echo "$SAY: binaries exist only for macOS (arm64 and x86_64); build with 'cargo build --release'" >&2
    exit 1
    ;;
esac

archive="${NAME}-${target}.tar.gz"
# O .sha256 da taiki-e/upload-rust-binary-action não leva a extensão do arquivo.
checksum="${NAME}-${target}.sha256"
# REVIEWQ_RELEASE_BASE troca a origem num teste (aceita file://).
base="${REVIEWQ_RELEASE_BASE:-https://github.com/${REPO}/releases/download}/${TAG}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# A CDN do GitHub pode dar 404 por alguns minutos num asset recém-publicado.
dl() {
  for _ in 1 2 3 4 5 6; do
    curl -fsSL "$1" -o "$2" && return
    sleep 3
  done
  return 1
}

echo "$SAY: downloading $archive ($TAG)"
dl "$base/$archive" "$tmp/$archive"
dl "$base/$checksum" "$tmp/$checksum"

echo "$SAY: verifying the checksum"
expected="$(awk '{print $1}' "$tmp/$checksum")"
actual="$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')"
if [ "$expected" != "$actual" ]; then
  echo "$SAY: checksum mismatch (expected $expected, got $actual)" >&2
  exit 1
fi

tar -xzf "$tmp/$archive" -C "$tmp"
new="$tmp/$NAME"
chmod 0755 "$new"

dest_dir="$HOME/.local/bin"
dest="$dest_dir/$NAME"
config_dir="$HOME/.config/herdr-reviewq"
plist="$HOME/Library/LaunchAgents/$LABEL.plist"

# O exemplo fica ao lado do config, já que a raiz do plugin não é um caminho para se lembrar.
mkdir -p "$config_dir"
cp "$ROOT/config.example.toml" "$config_dir/config.example.toml"

# Troca o arquivo de uma vez: um daemon rodando segue com o binário antigo até reiniciar.
install_binary() {
  mkdir -p "$dest_dir"
  cp "$new" "$dest.new"
  mv -f "$dest.new" "$dest"
}

if [ -f "$plist" ]; then
  # Serviço já instalado: `service install` copia o binário e reinicia o daemon na versão nova.
  if "$new" service install; then
    echo "$SAY: $TAG installed at $dest and daemon restarted"
  else
    install_binary
    echo "$SAY: $TAG installed at $dest, but the daemon did not restart; run: $NAME service install" >&2
  fi
  echo "$SAY: a panel that is already open keeps the old version until you close it and open it again"
else
  install_binary
  echo "$SAY: $TAG installed at $dest"
  echo "$SAY: still to configure:"
  echo "$SAY:   1. cp $config_dir/config.example.toml $config_dir/config.toml and edit the [[repos]]"
  echo "$SAY:   2. $dest service install"
fi
