# 公共引导：定位目录、准备 venv。被 run / extract-proto source。
SPIKE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENV="$SPIKE_DIR/.venv-$(uname -s)"
if [ ! -x "$VENV/bin/python" ]; then
  echo "[bootstrap] 创建 $VENV" >&2
  if command -v uv >/dev/null 2>&1; then
    uv venv -q "$VENV" && uv pip install -q --python "$VENV/bin/python" -r "$SPIKE_DIR/requirements.txt"
  else
    python3 -m venv "$VENV" && "$VENV/bin/pip" install -q -r "$SPIKE_DIR/requirements.txt"
  fi
fi
PY="$VENV/bin/python"
export PYTHONPATH="$SPIKE_DIR/lib"
