// Korean text for the codes the core and the capture provider speak in.
export const MARKER_KINDS: Record<string, string> = { topic: "주제", decision: "결정", question: "질문", risk: "위험", other: "기타" };
export const markerKind = (k: string): string => MARKER_KINDS[k] ?? k;

/** gap `reason` values of docs/capture-interface.md */
export const GAP_REASONS: Record<string, string> = {
  late_start: "시작이 늦음", end_pad: "끝부분 채움", idle: "소리 없음", no_data: "수신된 데이터 없음", dropout: "입력 끊김",
  discontinuity: "시간 불연속", write_failed: "디스크 쓰기 실패", conversion_failed: "변환 실패", overlap_trimmed: "겹침 제거", restart: "스트림 재시작",
};
export const gapReason = (r: string): string => GAP_REASONS[r] ?? r;

export const ERROR_HEADINGS: Record<string, string> = {
  permission_microphone: "마이크 권한이 필요합니다",
  permission_screen_capture: "화면 및 시스템 오디오 녹음 권한이 필요합니다",
  mic_device_not_found: "마이크를 찾을 수 없습니다",
  bad_out_dir: "녹음 폴더를 만들 수 없습니다",
  capture_start_failed: "녹음을 시작하지 못했습니다",
  stream_stopped: "오디오 스트림이 끊겨 다시 연결하는 중입니다",
  stream_lost: "오디오 스트림을 잃어 녹음을 마쳤습니다",
  write_failed: "디스크에 쓰지 못했습니다",
  finalize_failed: "녹음 파일을 마무리하지 못했습니다",
  bad_arguments: "녹음 프로그램 실행 인자가 잘못되었습니다",
};
export const errorHeading = (code: string): string => ERROR_HEADINGS[code] ?? `녹음 오류 (${code})`;

/** The core's English capture warnings, as far as the UI knows them; anything else is shown as is. */
export const WARNINGS: Record<string, string> = {
  "imported from an incomplete capture": "녹음이 정상적으로 끝나지 않아 마지막 확인 지점까지만 저장했습니다.",
  "the provider did not stop in time and was killed": "녹음 프로그램이 제때 멈추지 않아 강제로 종료했습니다.",
};
export const warning = (w: string): string => WARNINGS[w] ?? w;

/** `secret_access_denied` on a plugin run: macOS did not hand the saved API key to this app, so the
 *  plugin was not started. The prompt asks for the Mac's login password, which people do not expect. */
export const KEYCHAIN_DENIED_RUN = "맥이 키체인에 저장된 API 키를 꺼내도 되는지 물었는데 허용되지 않았어요. 그래서 플러그인을 실행하지 않았어요. 다시 실행하고, 창이 뜨면 이 맥의 로그인 암호를 넣은 뒤 ‘항상 허용’을 눌러 주세요.";
export const KEYCHAIN_DENIED_WHY = "그 창은 맥이 직접 띄우는 거예요. 키를 다른 meetko 프로그램(명령줄 meetko나 다른 빌드의 앱)으로 저장하면 이 앱이 처음 쓸 때 한 번 물어요. 플러그인 화면의 ‘API 키 넣기’에서 이 앱으로 다시 저장하면 묻지 않아요.";
/** `secret_access_denied` when saving or removing a key in the key dialog: another meetko program owns the item. */
export const KEYCHAIN_DENIED_CHANGE = "이 키는 다른 meetko 프로그램(명령줄 meetko나 다른 빌드의 앱)이 저장한 거라, 맥이 이 앱에서 바꾸는 걸 허용하지 않았어요. 창이 뜨면 이 맥의 로그인 암호를 넣고 허용하거나, 저장했던 프로그램에서 먼저 지운 뒤(명령줄: meetko plugin secret clear) 여기서 다시 저장해 주세요.";

/** core error codes (ApiError.code) whose raw English message is replaced by a plain Korean one */
export const API_ERRORS: Record<string, string> = {
  stale_draft: "이 초안은 현재 버전보다 오래되어 승인할 수 없어요. 가장 최근 초안을 선택해 승인하세요.",
  busy: "다른 작업이 진행 중이에요. 끝난 뒤 다시 눌러 주세요.",
  engine_sandbox: "이 컴퓨터에서는 Qwen3-ASR 엔진을 안전하게 격리해서 돌릴 수 없어서 멈췄어요. 설정 › 받아쓰기 준비에서 Whisper로 바꿔 주세요.",
};
export const apiErrorText = (code: string, fallback: string): string => API_ERRORS[code] ?? fallback;

/** export failures; anything unlisted gets the last line, not the core's English */
export const EXPORT_ERRORS: Record<string, string> = {
  export_write_failed: "고른 위치에 파일을 쓸 수 없어요. 다른 폴더를 골라 다시 저장해 주세요.",
  not_found: "이 버전을 찾을 수 없어요. 회의를 다시 열어 주세요.",
  corrupt_data: "이 버전의 전사 데이터가 손상돼서 내보낼 수 없어요. 다른 버전을 골라 주세요.",
  clipboard: "클립보드에 쓸 수 없었어요. 다시 시도하거나 같은 메뉴에서 파일로 저장해 주세요.",
  busy: "저장 창이 이미 열려 있어요. 그 창을 먼저 닫아 주세요.",
};
export const exportErrorText = (code: string): string => EXPORT_ERRORS[code] ?? "잠시 후 다시 시도해 주세요.";

/** Making a draft from the live captions: every code it can end with, in plain Korean. Kept apart from
 *  API_ERRORS because the same codes mean other things elsewhere (approve, batch); no raw English. */
export const LIVE_DRAFT_ERRORS: Record<string, string> = {
  ...API_ERRORS,
  not_found: "이 회의에는 실시간 자막이 없어요.",
  invalid_data: "실시간 자막이 초안 형식에 맞지 않아 초안을 만들지 못했어요.",
  corrupt_data: "저장된 실시간 자막이나 녹음 파일이 손상돼 초안을 만들지 못했어요.",
  stale_draft: "이 회의에는 이미 초안이 있어요.",
};
/** The automatic 다시 받아쓰기 after a recording: every code it can end with, in plain Korean (never the
 *  core's English). The live draft is kept whatever happens; the screen says so next to this. */
export const RETRANSCRIBE_ERRORS: Record<string, string> = {
  busy: API_ERRORS.busy,
  engine_sandbox: API_ERRORS.engine_sandbox,
  model_missing: "받아쓰기 모델을 찾지 못했어요. 설정 › 받아쓰기 준비에서 확인해 주세요.",
  engine_not_found: "받아쓰기 엔진을 찾지 못했어요. 설정 › 받아쓰기 준비에서 확인해 주세요.",
  bad_engine_output: "받아쓰기 엔진이 알아볼 수 없는 결과를 냈어요.",
  corrupt_data: "녹음 파일이 저장할 때와 달라져서 다시 받아쓰지 않았어요.",
  invalid_data: "받아쓴 결과가 초안 형식에 맞지 않았어요.",
  not_found: "이 회의를 찾을 수 없어요.",
  // the core's one timeout code, also used for a batch engine that ran past its time budget
  plugin_timeout: "받아쓰기가 정해진 시간 안에 끝나지 않아 멈췄어요.",
};
export const retranscribeErrorText = (code: string): string => RETRANSCRIBE_ERRORS[code] ?? "예상하지 못한 문제로 멈췄어요.";

export const liveDraftErrorText =(code: string): string => LIVE_DRAFT_ERRORS[code] ?? "예상하지 못한 문제로 초안을 만들지 못했어요. 다시 눌러 보세요.";
