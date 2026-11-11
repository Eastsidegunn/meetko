"""Summary templates (own wording). Every template maps onto the same JSON shape:
one_line, points, decisions, actions, extra. Only the Korean instructions and labels differ."""
import re

BASE = {
    "name": "기본 회의록",
    "instruction": (
        "일반적인 회의 기록입니다. 회의 전체를 한 문장으로 요약하고, 논의한 요점, "
        "확정된 결정 사항, 앞으로 할 일(담당 화자와 기한)을 정리하세요. extra 는 빈 목록으로 두세요."
    ),
    "labels": {"one_line": "한 줄 요약", "points": "논의 요점", "decisions": "결정 사항", "extra": "참고"},
}
ONE_ON_ONE = {
    "name": "1:1 면담",
    "instruction": (
        "두 사람이 나눈 1:1 대화입니다. 전체를 한 문장으로 요약하고, 나눈 이야기를 points 에 요점으로 정리하고, "
        "서로 합의한 것을 decisions 에 넣고, 다음 만남까지 할 일(담당·기한)을 actions 에 넣으세요. "
        "짚고 갈 고민이나 피드백은 extra 에 넣으세요."
    ),
    "labels": {"one_line": "한 줄 요약", "points": "나눈 이야기", "decisions": "합의한 것", "extra": "고민·피드백"},
}
TECH_REVIEW = {
    "name": "기술 리뷰·장애 회고",
    "instruction": (
        "기술 리뷰 또는 장애 회고 회의입니다. 전체를 한 문장으로 요약하고, 무슨 일이 있었는지 "
        "시간 순서의 요점을 points 에 넣고, 원인이나 쟁점은 extra 에 넣고, 확정된 결정은 decisions 에, "
        "후속 조치(담당·기한)는 actions 에 넣으세요."
    ),
    "labels": {"one_line": "한 줄 요약", "points": "무슨 일이 있었나", "decisions": "결정 사항", "extra": "원인·쟁점"},
}
TEMPLATES = {"default": BASE, "one_on_one": ONE_ON_ONE, "tech_review": TECH_REVIEW}

# Latin keywords need non-letter neighbours ("preview" is not "review"); "1:1" must not sit inside
# digits ("11:15", "21:10"); "1on1" must not be followed by letters/digits ("1 on 1st").
_ONE = re.compile(r"(?<!\d)1\s*:\s*1(?!\d)|(?<!\d)1\s*-?\s*on\s*-?\s*1(?![0-9a-z])|원온원|면담", re.I)
_TECH = re.compile(r"회고|장애|포스트\s*모템|리뷰|설계|(?<![a-z])(?:post-?mortems?|incidents?|retro(?:spective)?s?|reviews?|rfcs?)(?![a-z])", re.I)


def pick_template(title):
    t = title or ""
    if _ONE.search(t):
        return ONE_ON_ONE
    if _TECH.search(t):
        return TECH_REVIEW
    return BASE
