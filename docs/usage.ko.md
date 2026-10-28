# 사용법

[English](usage.md) | 한국어 | [简体中文](usage.zh-CN.md)

> 이 문서는 [usage.md](usage.md)(커밋 ffa5953)의 번역이다. 내용이 다르면 영어 원문을 따른다.

이 문서는 `v12n` 명령과 출력 형식, 종료 코드, HTTP 엔드포인트와 그 설정을 설명한다. 전체 개요는 [README](../README.ko.md)에서, 쿼리 결과를 결정하는 규칙은 [semantics.ko.md](semantics.ko.md)에서 설명한다.

## `v12n query`

`v12n query mapping.ttl query.rq --db postgresql://reader@localhost/shop`는
SPARQL 쿼리를 실행하고 결과를 다음 세 형식 중 하나로 출력한다.

- SPARQL 1.1 Query Results JSON.
- `--format csv`를 지정하면 SPARQL 1.1 Query Results CSV. CSV에는 값의
  문자열 표기(lexical form)만 들어가며, 바인딩(binding)되지 않은 변수는 빈
  필드가 된다.
- `--format table`을 지정하면 표.

쿼리는 다음 세 곳에서 읽는다.

- 위 예의 `query.rq` 같은 파일
- 파일 이름 대신 `-`를 지정하면 stdin
- `--query '<SPARQL>'`를 지정하면 명령줄

이 형식들에는 결과가 부분 결과임을 표시하는 방법이 없다. 따라서 명령은 제한에
도달한 사실을 결과와 별도로 stderr와 종료 코드로 알린다.

| 종료 코드 | 조건 |
|---|---|
| `0` | 성공. 결과가 정확히 `--row-limit` 행인 경우도 포함한다 |
| `1` | 잘못된 입력: 인자, 파일, 쿼리 문법, 지원하지 않는 SPARQL |
| `2` | 잘못된 매핑 |
| `3` | 데이터베이스 오류, 또는 RDF term으로 변환할 수 없는 값 |
| `4` | statement timeout(`--timeout`). 첫 행 이전에 발생한 타임아웃은 다른 오류와 같은 방식으로 보고한다. 출력이 시작된 뒤 발생한 타임아웃은 stdout의 JSON을 닫지 않은 상태로 남긴다(CSV에는 끝 표시가 없다) |
| `5` | 행 제한 도달(`--row-limit`). stdout에는 처음 `--row-limit`개의 해(solution)가 완결된 결과로 들어 있고, stderr에는 경고가 출력된다 |

stderr에 출력되는 줄은 원인과 제한값을 명시한다. 예를 들어 결과가 잘리면
`warning: result truncated at the row limit of 10000; more rows exist ...`를
출력하고, 타임아웃이 발생하면
`error: statement timeout of 30s reached after 120 rows; ...`를 출력한다.

## 에이전트용 CLI

CLI는 사람이 직접 실행하는 경우뿐 아니라 셸 도구가 호출하는 경우도 고려해
설계했다. CLI는 다음 규칙을 따른다.

- 대화형 입력 없이 실행된다.
- stdout이 터미널이 아니면 항상 JSON을 출력한다. `--format json`을 지정하면
  JSON 출력을 강제한다.
- 색상은 터미널에서만 사용하며, `NO_COLOR`가 설정되어 있으면 사용하지 않는다.
- 모든 명령이 위의 종료 코드를 사용한다.

호출자는 다음 세 단계를 거쳐 매핑에서 쿼리 결과를 얻는다.

1. `v12n terms mapping.ttl`은 매핑이 노출하는 클래스와 속성을 나열한다. 목록에
   없는 클래스나 속성으로는 일치하는 결과를 얻을 수 없다.

   ```json
   {"prefixes":{"ex":"http://example.org/"},
    "classes":[{"iri":"http://example.org/Customer","prefixed_name":"ex:Customer","triples_maps":["<#CustomerMap>"]}, ...],
    "properties":[{"iri":"http://example.org/name","prefixed_name":"ex:name","triples_maps":["<#CustomerMap>"]}, ...]}
   ```

2. `v12n explain mapping.ttl --query '<SPARQL>'`은 데이터베이스 없이 쿼리를
   검사한다. 문법 오류가 있거나 지원하지 않는 기능을 사용하면 1로 종료한다.
   성공하면 SQL과 바인드 파라미터, 투영되는 변수를 출력한다.

   ```json
   {"sql":"SELECT DISTINCT t0.\"id\" AS \"customer\", ...","params":["VIP"],
    "variables":[{"name":"customer","term_type":"iri","columns":["customer"]}, ...]}
   ```

3. `v12n query mapping.ttl --query '<SPARQL>'`는 쿼리를 실행하고 SPARQL 1.1
   Query Results JSON을 출력한다. 데이터베이스는 `--db`나 `V12N_DATABASE_URL`로
   지정한다. 행 제한, statement timeout, 읽기 전용 세션은 기본으로 활성화되어
   있다.

출력 전에 발생한 오류는 결과 대신 JSON 객체 하나로 stdout에 출력된다. 따라서
호출자는 종료 코드를 확인한 뒤 성공 여부와 관계없이 stdout을 읽는다.

```json
{"ok":false,"error":{"kind":"query","message":"query syntax error: ...","exit_code":1}}
```

아래 표는 오류 `kind`별 종료 코드와, 해당하는 경우 `details`를 나열한다.

| `kind` | 종료 코드 | `details` |
|---|---|---|
| `input`, `query`, `unsupported`, `output`, `runtime` | 1 | 파일을 읽을 수 없을 때 `{"path": ...}` |
| `mapping` | 2 | |
| `database`, `data` | 3 | |
| `timeout` | 4 | `{"timeout_ms": ..., "rows": ...}` |

일부 오류는 stdout 대신 stderr에 `error: ...` 한 줄로 출력된다.

- 결과 출력이 시작된 뒤 발생한 오류. 이 시점에는 stdout을 더 이상 바꿀 수 없다
- `--format csv`나 `--format table`을 지정했을 때 발생한 모든 오류

행 제한 `warning: ...`(종료 코드 5)은 항상 stderr로 출력된다.

`v12n <command> --help`는 각 명령의 JSON과 CSV 형태를 나열한다. 도움말의 예제는
저장소 루트에서 실행하는 것을 기준으로 한다.

## HTTP 엔드포인트

`v12n serve --config v12n.toml`은 `/sparql`에서 SPARQL 1.1 Protocol을 제공한다.
생존 확인(liveness check)용 `/healthz`도 제공한다.

요청은 다음 세 방식 중 하나로 쿼리를 전달한다.

- `GET /sparql?query=...`
- `application/x-www-form-urlencoded`(`query=...`)를 사용하는 `POST`
- `application/sparql-query`를 사용하고 쿼리를 본문으로 보내는 `POST`

결과는 SPARQL 1.1 Query Results JSON(`application/sparql-results+json`)이다.
결과는 PostgreSQL에서 행이 도착하는 대로 스트리밍된다. 다른 형식을 요청하면
`406`을 반환한다.

데이터셋은 매핑이 정의하므로, `default-graph-uri`와 `named-graph-uri`
파라미터를 지정하면 `400`을 반환한다.

| 상태 | 조건 |
|---|---|
| `200` | 결과. 행 제한이 적용되어 결과가 잘리면 응답에 `V12N-Truncated: <row limit>`가 포함되고, 응답은 정확히 그 수만큼의 해를 담는다. 같은 경우 `v12n query`는 5로 종료한다 |
| `400` | 쿼리 문법 오류, `query`가 없거나 두 번 이상 지정된 경우, 데이터셋 파라미터, 너무 많은 SQL branch로 unfold되는 쿼리 |
| `406` / `413` / `414` / `415` | `406`: 제공하지 않는 형식. `413`: `POST`에서 `max_query_bytes`를 넘는 쿼리. `414`: `GET`에서 `max_query_bytes`를 넘는 쿼리. `415`: 그 밖의 content type |
| `501` | 아직 지원하지 않는 SPARQL 기능 |
| `503` | 첫 행 이전에 발생한 statement timeout(이 경우 `v12n query`는 4로 종료한다), 또는 `max_concurrent_requests` 도달(응답에 `Retry-After`가 포함된다) |
| `500` | 데이터베이스 오류나 그 밖의 내부 오류 |

상태 코드는 본문보다 먼저 전송된다. 따라서 첫 행 이후에 발생한 오류는 상태
코드를 바꿀 수 없다. 이 경우 서버는 응답을 중단하고 오류를 로그에 기록하며,
JSON은 닫히지 않은 상태로 남는다.

결과가 잘리는지는 첫 행을 보내기 전에 알 수 있다. PostgreSQL이 행을 보내기 전에
제한까지의 행 수를 세기 때문이다.

설정은 `--config`로 지정한 TOML 파일에서 읽는다. 예는 다음과 같다.

```toml
[server]
address = "127.0.0.1"
port = 7878

[database]
url = "postgresql://reader@localhost/shop"
password_env = "SHOP_DB_PASSWORD"   # optional: read the password from this variable

[mapping]
path = "mapping.ttl"                # relative to this file
base_iri = "http://data.example.com/"   # optional: for relative IRIs (see semantics.md)

[limits]
statement_timeout = "30s"           # 0 disables
row_limit = 10000                   # 0 disables
max_query_bytes = 65536
max_concurrent_requests = 16        # more are refused with 503, not queued
```

환경 변수는 파일의 설정을 덮어쓴다. 다음 표는 각 설정에 대응하는 환경 변수와
기본값을 나열한다.

| 설정 | 환경 변수 | 기본값 |
|---|---|---|
| `[server] address` | `V12N_ADDRESS` | `127.0.0.1` |
| `[server] port` | `V12N_PORT` | `7878` |
| `[database] url` | `V12N_DATABASE_URL` | 필수 |
| `[database] password_env` | (파일 전용) | 없음 |
| `[mapping] path` | `V12N_MAPPING` | 필수 |
| `[mapping] base_iri` | `V12N_BASE_IRI` | 없음: 상대 IRI는 데이터 오류 |
| `[limits] statement_timeout` | `V12N_STATEMENT_TIMEOUT` | `30s` |
| `[limits] row_limit` | `V12N_ROW_LIMIT` | `10000` |
| `[limits] max_query_bytes` | `V12N_MAX_QUERY_BYTES` | `65536` |
| `[limits] max_concurrent_requests` | `V12N_MAX_CONCURRENT_REQUESTS` | `16` |

다른 명령도 같은 변수 중 일부를 읽는다.

- `v12n query`는 `V12N_DATABASE_URL`, `V12N_ROW_LIMIT`,
  `V12N_STATEMENT_TIMEOUT`, `V12N_BASE_IRI`를 읽는다. 명령줄 옵션이 이
  변수들보다 우선한다.
- `v12n explain`은 `V12N_BASE_IRI`를 읽는다.

`serve`에서는 `--base-iri` 옵션도 base IRI 설정을 덮어쓴다. `serve`는 그 밖의
`V12N_*` 변수를 무시하고, 변수마다 경고를 로그에 기록한다. 따라서 변수 이름을
잘못 입력해도 확인할 수 있다.

읽기 권한만 있는 데이터베이스 사용자로 접속한다. 엔진도 세션을 읽기 전용
트랜잭션으로 설정한다.

데이터베이스 URL과 비밀번호는 로그에 기록하지 않는다. 요청 로그는 `RUST_LOG`로
지정한 레벨에 따라 stderr에 기록된다. 요청 로그에는 생성된 SQL과 소요 시간이
포함되며, 결과 값은 포함되지 않는다.

TLS, 인증, CORS는 서버 앞단의 리버스 프록시가 담당한다.
