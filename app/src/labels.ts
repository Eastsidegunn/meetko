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

/** core error codes (ApiError.code) whose raw English message is replaced by a plain Korean one */
export const API_ERRORS: Record<string, string> = {
  stale_draft: "이 초안은 현재 버전보다 오래되어 승인할 수 없어요. 가장 최근 초안을 선택해 승인하세요.",
  busy: "다른 작업이 진행 중이에요. 끝난 뒤 다시 눌러 주세요.",
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
export const liveDraftErrorText = (code: string): string => LIVE_DRAFT_ERRORS[code] ?? "예상하지 못한 문제로 초안을 만들지 못했어요. 다시 눌러 보세요.";
