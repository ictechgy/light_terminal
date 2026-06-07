# PoC3 — DECSTBM 손상→self-heal 수렴 계측 + 입력칸 회귀 가드

## 목적
일반 터미널 + 메인버퍼 에이전트(codex)에서 status row가 **best-effort**로만 안전하다는
연구 결론(memory §B)을 수치로 확인한다. 보증 수준은 "무손상"이 아니라 **"손상 후 ~50ms 내
수렴"**(STATUS_DAMAGE_HEARTBEAT). 동시에 content-only 백스톱 적용 후 **입력칸 증식 회귀가
재발하지 않는지** 확인한다.

## 1) 자동(이미 CI로 증명됨 — 새 코드 불필요)
아래 단위 테스트가 로직 불변식을 고정한다. `PATH="$HOME/.cargo/bin:$PATH" cargo test --locked -- --test-threads=1`:

| 불변식 | 테스트 (src/client.rs) |
|---|---|
| 손상(ED `CSI J` / DECSTBM-reset / RIS) 감지 | `terminal_output_tracker_marks_status_damage_for_screen_erases_and_scroll_resets` (9409) |
| chunk 경계 분할 손상도 감지 | `terminal_output_tracker_marks_status_damage_across_chunk_boundaries` (9464) |
| 손상 시 50ms fast-lane 발화 + rate-limit | `heartbeat_damage_fast_lane_fires_at_short_interval_and_rate_limits` (9749) |
| fast-lane(50ms)이 forced 백스톱(2s)보다 빠름 | `heartbeat_damage_fast_lane_is_faster_than_forced_backstop` (9771) |
| **content-only 백스톱은 reserve(DECSTBM) 미방출 = codex scroll-region 보존(입력칸 가드)** | `refresh_content_only_backstop_omits_reserve` (신규) |

→ "손상 감지 → ~50ms 수렴", "백스톱은 codex scroll-region 불간섭"은 코드로 증명됨.
**색 손상·입력칸 증식은 셀 그리드 시각 현상이라 CI로 못 잡으므로 아래 라이브 검증 필수.**

## 2) 라이브 검증(데몬 재시작 필요 — 시각/계측)
lterm 데몬은 핫스왑이 안 되므로 새 release 바이너리(`target/release/lterm`, content-only fix 포함)를
쓰려면 **기존 데몬/세션을 재시작**해야 한다.

### A. 입력칸 회귀(가장 중요)
1. 새 lterm 세션에서 codex 실행.
2. 다른 cmux 윈도우로 갔다가 **돌아온다**(이전에 입력칸이 늘어나던 트리거).
3. 확인: codex 하단 입력칸이 **늘어나지 않는가?** (content-only 백스톱이 reserve를 안 쏴서
   codex scroll-region을 보존 → 입력칸 안정 기대)

### B. 수렴시간 체감
1. codex가 busy 출력(긴 응답 스트리밍) 중일 때 status row가 깜빡여도 **~50ms 안에 안정**되는가?
2. 화면을 강제로 흔들기(아래 합성 손상 주입)로 수렴을 눈으로 본다.

### C. 합성 손상 주입(선택, 정밀 관찰)
codex 없이 status row 동작만 보려면, lterm 세션에서 ED를 한 번 쏴 손상→self-heal을 유발:
```bash
printf '\033[2J'   # 전체 화면 지우기(ED) → status row 손상 → 다음 heartbeat가 reserve 포함 redraw로 복구
```
status row가 사라졌다가 즉시(다음 50ms fast-lane 또는 2s 백스톱) 되돌아오면 self-heal 정상.

## 3) 정직한 한계(연구 §B)
- 일반 터미널 + 메인버퍼 codex에서 **무손상 보장은 원리적으로 불가능**. 위 검증이 "통과"여도
  보증은 "~50ms 수렴"이지 "절대 안 깜빡임"이 아니다.
- 진짜 무손상을 원하면 환경별 안전 백엔드(cmux surface / iTerm native / tmux status-line)로
  라우팅(PoC1 `select_status_backend`)하고, plain+에이전트는 row를 떼지 말고 타이틀 위임을
  유지하는 것이 정답.
