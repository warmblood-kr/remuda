# Lua 확장 기능

<nav aria-label="Language"><a href="lua.html" lang="en">English</a> ·
<a href="ko-lua.html" lang="ko">한국어</a></nav>

remuda는 데몬 수명 동안 하나의 Lua 이미지를 유지합니다. Rust는 호스트
기능을 제공하고, Lua는 그 위에 확장 기능의 어휘를 구성합니다. 내장
바인딩과 `remuda.tool`로 등록한 도구는 하나의 런타임 레지스트리에
메타데이터를 기록합니다.

## 명령

```sh
remuda doc
remuda doc --format markdown
remuda doc --format json
remuda mod list --format json
```

기본 문서 형식은 reStructuredText입니다. Markdown과 JSON 출력은 다른
문서 사이트나 도구에서 사용할 수 있습니다. `remuda mod list`는 설치된
확장 기능의 이름, 버전, API, 진입점, 출처와 상태를 실제 매니페스트에서
읽어 출력합니다.

## 생성된 레퍼런스

아래 내용은 소스에 별도로 작성한 목록이 아니라 현재 remuda 런타임
레지스트리에서 생성한 HTML입니다.

<div class="generated-reference">
{% include_relative lua-reference.html %}
</div>

[RST 원문](lua-reference.rst)도 내려받을 수 있습니다. Rust 바인딩이나 Lua
등록 정보를 변경한 뒤 저장소 루트에서 다음 명령을 실행하면 두 산출물이
함께 갱신됩니다.

```sh
sh scripts/generate-lua-reference.sh
```
