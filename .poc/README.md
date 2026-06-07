# .poc/ — status row cross-env 라우팅 수동 검증 산출물

이 디렉터리는 **일회성 라이브/시각 검증 전용** 스크립트·가이드다(제품 코드 아님).
`select_status_backend`(cross-env 백엔드 라우팅) **배선이 끝나면 제거**해도 된다.

- `poc2-iterm-status.sh` — iTerm2 네이티브 status bar(OSC 1337 SetUserVar) 라이브 검증.
  NativeChrome 백엔드가 codex 풀스크린 TUI와 무충돌로 갱신되는지 육안 확인용.
- `poc3-convergence.md` — DECSTBM 손상→self-heal 수렴 + 입력칸 회귀 가드 검증 가이드.
  로직 불변식은 단위테스트(테스트 함수명 참조)로 CI 증명, 시각/수렴은 데몬 재시작 후 육안.

자동 CI 검증은 `cargo test`(단위/통합)가 담당한다. 이 디렉터리는 그걸 보완하는
사람-개입 검증 절차의 재현 스크립트일 뿐이다.
