# remuda 한국어 안내

<nav aria-label="Language"><a href="index.html" lang="en">English</a> ·
<a href="ko.html" lang="ko">한국어</a></nav>

remuda는 코딩 에이전트 세션을 여러 개 실행하고 전환하는 터미널
오케스트레이터입니다. 하나의 데몬이 Lua 런타임을 유지하므로 세션과 확장
기능이 같은 실행 이미지에서 동작합니다.

## 문서

- [Lua 확장 기능 안내](ko-lua.md)
- [영문 Lua 확장 기능 안내](lua.md)
- [Butler 안내](butler.md)
- [설계 문서](design.md)

확장 기능 목록은 수동으로 관리하지 않습니다. 현재 바이너리에 포함된
매니페스트는 다음 명령으로 확인할 수 있습니다.

```sh
remuda mod list --format json
```

Lua 런타임 문서도 같은 레지스트리에서 생성됩니다.

```sh
remuda doc
remuda doc --format markdown
remuda doc --format json
```
