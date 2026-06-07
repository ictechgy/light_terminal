#!/usr/bin/env bash
# PoC2 — iTerm2 네이티브 status bar(OSC 1337 SetUserVar) 라이브 검증
#
# 목적: 셀 그리드 밖 iTerm2 status bar에 외부 프로세스가 OSC 1337로 데이터를 주입할 때,
#       codex 같은 풀스크린 TUI가 같은 창에서 돌고 있어도 status가 갱신되고 색/커서/입력칸
#       손상이 없는지(=scroll-region 무개입) 육안 확인한다.
#
# 사전 설정(iTerm2, 1회):
#   1) iTerm2 > Settings > Profiles > Session > Status bar enabled 체크 > Configure Status Bar
#   2) "Interpolated String" 컴포넌트를 status bar로 드래그
#   3) 그 컴포넌트 편집에서 수식: \(user.lterm)
#   (선택) Auto-update 주기는 iTerm이 user var 변경 시 즉시 갱신하므로 별도 설정 불필요.
#
# 실행:
#   ./poc2-iterm-status.sh           # 2초마다 가짜 lterm status를 status bar에 주입(Ctrl-C로 종료)
#   ./poc2-iterm-status.sh --once    # 1회만 주입
#
# 검증 포인트:
#   - status bar에 'lt:api:%3 codex 78%' 류 텍스트가 뜨고 2초마다 카운터가 증가하는가?
#   - 같은 창에서 codex(또는 임의 풀스크린 TUI: vim/htop)를 띄운 채로도 status가 갱신되는가?
#   - codex 화면(색/커서/하단 입력칸)에 아무 손상이 없는가? (scroll-region 무개입이면 무손상)
#
# 주의: OSC 1337 SetUserVar 값은 base64 plain text다. truecolor ANSI나 멀티라인은
#       iTerm status bar 컴포넌트가 직접 렌더하지 않는다(연구 §A 결론). 색은 컴포넌트 자체
#       스타일로만, 멀티라인은 불가.

set -euo pipefail

emit_user_var() {
  # $1 = 표시할 plain 문자열. OSC 1337 ; SetUserVar=lterm=<base64> BEL
  local value_b64
  value_b64="$(printf '%s' "$1" | base64 | tr -d '\n')"
  printf '\033]1337;SetUserVar=lterm=%s\007' "$value_b64"
}

if [[ "${1:-}" == "--once" ]]; then
  emit_user_var "lt:api:%3 codex 78% (poc2 once)"
  echo "[poc2] user.lterm 1회 주입 완료. iTerm status bar의 \\(user.lterm) 확인."
  exit 0
fi

echo "[poc2] 2초 간격으로 user.lterm 주입 시작. iTerm status bar를 보세요. Ctrl-C로 종료."
echo "[poc2] (같은 창에서 codex/vim을 띄운 채로도 손상 없이 갱신되는지 확인)"
i=0
while true; do
  i=$((i + 1))
  # 실제로는 understatus 출력에서 핵심 요약을 뽑아 넣게 된다(NativeChrome 백엔드).
  emit_user_var "lt:api:%3 codex 78% · tick ${i}"
  sleep 2
done
