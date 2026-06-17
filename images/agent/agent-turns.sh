#!/bin/bash
# The agent turn-driver: guestd (wrap mode) execs this as PID 1's child and infers
# turn boundaries from the @@TURN_START@@ / @@TURN_END@@ markers it prints around
# each agent step. Between turns it idles — that idle window is the verified-
# quiescence period a migration is allowed to land in. The whole process (with
# aider's in-RAM state and the working tree) rides the snapshot to the new host.
#
# The model API key arrives in the environment as AGENT_API_KEY (handed over by
# the host via the Secrets vsock message, because /etc/sleepwalk/wrap-await-secrets
# is present in this rootfs). It is never baked into the image.
set -u

# guestd (PID 1) execs us with a near-empty environment — no PATH/HOME — so set
# them before running aider (its imports and git read both).
export PATH="${PATH:-/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin}"
export HOME="${HOME:-/root}"
export GROQ_API_KEY="${AGENT_API_KEY:-}"
MODEL="${AGENT_MODEL:-groq/llama-3.3-70b-versatile}"
GAP="${AGENT_GAP_SECS:-20}"
REPO="${AGENT_REPO:-/root/task}"

cd "$REPO" || { echo "[agent] no repo at $REPO"; exit 1; }
if [[ -z "$GROQ_API_KEY" ]]; then
    echo "[agent] AGENT_API_KEY empty — secrets not delivered; aborting"
    exit 1
fi

# A multi-turn coding task: each prompt is one turn (one migration-eligible unit).
PROMPTS=(
    "Create calc.py with a function add(a, b) that returns a + b, with a docstring."
    "Add subtract(a, b) to calc.py."
    "Add multiply(a, b) to calc.py."
    "Add divide(a, b) to calc.py that raises ValueError on a zero divisor."
    "Write test_calc.py with pytest tests covering add, subtract, multiply, and divide (including the zero-divisor case)."
)

aider_step() { # prompt
    aider --model "$MODEL" \
          --yes --no-auto-commits --no-stream --no-pretty \
          --no-check-update --no-show-model-warnings --no-gitignore \
          --message "$1" calc.py test_calc.py 2>&1 | sed -u 's/^/[aider] /'
}

turn=0
for p in "${PROMPTS[@]}"; do
    turn=$((turn + 1))
    echo "@@TURN_START@@"
    echo "[agent] turn $turn: $p"
    aider_step "$p"
    echo "@@TURN_END@@"
    echo "[agent] idle ${GAP}s (migration window)"
    sleep "$GAP"
done

echo "[agent] task complete: $turn turns; final tree:"
git -C "$REPO" status --porcelain | sed -u 's/^/[agent] /'
# Verify the work actually runs (zero-errors check for O6).
if command -v pytest >/dev/null 2>&1 && [[ -f "$REPO/test_calc.py" ]]; then
    ( cd "$REPO" && python3 -m pytest -q 2>&1 | sed -u 's/^/[agent] /' ) || true
fi
