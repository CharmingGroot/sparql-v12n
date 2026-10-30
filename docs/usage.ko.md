# 사용법

[English](usage.md) | 한국어 | [简体中文](usage.zh-CN.md)

> 이 문서는 [usage.md](usage.md)(커밋 8e6fa66)의 번역이다. 내용이 다르면 영어 원문을 따른다.

이 문서는 `v12n` 명령과 출력 형식, 종료 코드, HTTP 엔드포인트와 그 설정을 설명한다. 전체 개요는 [README](../README.ko.md)에서, 쿼리 결과를 결정하는 규칙은 [semantics.ko.md](semantics.ko.md)에서 설명한다.

## `v12n query`

`v12n query mapping.ttl query.rq --db postgresql://reader@localhost/shop`는
SPARQL 쿼리를 실행하고 결과를 다음 다섯 형식 중 하나로 출력한다.

- `--format json`을 지정하면 SPARQL 1.1 Query Results JSON.
- `--format xml`을 지정하면 SPARQL Query Results XML.
- `--format csv`를 지정하면 SPARQL 1.1 Query Results CSV. CSV에는 값의
  문자열 표기(lexical form)만 들어가며, 데이터타입과 언어 태그는 들어가지 않는다.
- `--format tsv`를 지정하면 SPARQL 1.1 Query Results TSV. TSV는 각 term을
  SPARQL 문법으로 적는다. IRI는 `<>`로 감싸고, 리터럴은 따옴표로 감싼 뒤 언어
  태그나 데이터타입을 붙이며, `123` 같은 숫자는 따옴표 없이 적는다.
- `--format table`을 지정하면 표.

CSV, TSV, 표에서 바인딩(binding)되지 않은 변수는 빈 필드가 된다. JSON과 XML은
해(solution)에서 그 변수를 생략한다. 표를 제외한 모든 형식은 행이 도착하는
대로 출력된다. XML 1.0은 탭, 줄 바꿈, 캐리지 리턴 외의 제어 문자를 담을 수
없으므로, U+0001 같은 문자가 든 리터럴을 만나면 XML 출력은 오류로 끝난다.
`--format xml`과 `--format tsv`는 `v12n query`에만 적용된다. 다른 명령은 이
형식을 종료 코드 1로 거절한다.

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
| `4` | statement timeout(`--timeout`). 첫 행 이전에 발생한 타임아웃은 다른 오류와 같은 방식으로 보고한다. 출력이 시작된 뒤 발생한 타임아웃은 stdout의 JSON이나 XML을 닫지 않은 상태로 남긴다(CSV와 TSV에는 끝 표시가 없다) |
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
- JSON이 아닌 형식에서 발생한 모든 오류

행 제한 `warning: ...`(종료 코드 5)은 항상 stderr로 출력된다.

`v12n <command> --help`는 각 명령의 JSON과 CSV 형태를 나열한다. 도움말의 예제는
저장소 루트에서 실행하는 것을 기준으로 한다.

## 데이터베이스 연결

`--db`, `V12N_DATABASE_URL`, `[database] url`은 libpq 연결 URI(예:
`postgresql://reader@db.example.com:5432/shop?sslmode=verify-full`)나
`key=value` 형식의 연결 문자열(예:
`host=db.example.com dbname=shop user=reader sslmode=verify-full`)을 받는다.

### TLS

`sslmode` 파라미터는 연결에 TLS를 사용할지와 서버 인증서를 어떻게 검사할지를
정한다. 각 값의 의미는 libpq와 같다.

| `sslmode` | TLS | 인증서 검사 |
|---|---|---|
| `disable` | 사용하지 않음 | 없음 |
| `allow` | TLS 없는 세션을 서버가 거절하면 사용 | 없음. `sslrootcert`에 파일을 지정하면 체인 검사 |
| `prefer`(기본값) | 서버가 지원하면 사용 | 없음. `sslrootcert`에 파일을 지정하면 체인 검사 |
| `require` | 항상 사용 | 없음. `sslrootcert`에 파일을 지정하면 체인 검사 |
| `verify-ca` | 항상 사용 | 인증서가 신뢰하는 CA로 이어지는지 검사 |
| `verify-full` | 항상 사용 | `verify-ca`의 검사에 더해, 인증서에 `host`의 호스트 이름이 있는지 검사 |

`prefer`와 `require`는 연결을 암호화하지만, `sslrootcert`에 파일을 지정하지
않으면 어떤 인증서든 받아들인다. 따라서 도청은 막지만 데이터베이스로 위장한
서버는 막지 못한다. 직접 관리하지 않는 네트워크에서는 `verify-full`을 사용한다.

`allow`는 TLS 없이 연결하고, 서버가 이 연결에 오류로 응답할 때에만 TLS로 두 번째
연결을 연다. 예를 들어 `pg_hba.conf`가 해당 사용자를 `hostssl`로만 허용하는
경우다. 네트워크 오류에서는 다시 시도하지 않는다. 두 번째 연결도 실패하면 그
오류를 보고한다.

`sslrootcert` 파라미터는 신뢰하는 CA를 지정한다.

- 지정하지 않으면 운영체제의 루트 인증서를 사용한다. Linux에서는 OpenSSL이
  사용하는 파일이며, 환경 변수 `SSL_CERT_FILE`과 `SSL_CERT_DIR`로 대체할 수 있다.
- 경로를 지정하면 CA 인증서 하나 이상을 담은 PEM 파일을 사용한다. 자체 CA를
  쓰는 서비스에 사용하며, 파일은 연결을 열 때 읽는다.
- `system`을 지정하면 운영체제의 루트 인증서를 사용한다. 이때 `sslmode`의
  기본값은 `verify-full`이 되고, libpq와 마찬가지로 더 약한 모드는 거절한다.

```sh
# A managed service whose certificate chains to a public CA
v12n query mapping.ttl query.rq \
  --db 'postgresql://reader@db.example.com/shop?sslmode=verify-full'

# A server with its own CA
v12n query mapping.ttl query.rq \
  --db 'postgresql://reader@db.internal/shop?sslmode=verify-full&sslrootcert=/etc/v12n/db-ca.pem'
```

`sslcert`와 `sslkey` 파라미터는 클라이언트 인증서와 그 개인 키를 지정한다.
인증서로 클라이언트를 인증하는 서버(`pg_hba.conf`의 `cert`, 또는
`clientcert=verify-ca`나 `verify-full`)에 사용한다.

- `sslcert`: 클라이언트 인증서와 그 뒤에 필요한 중간 인증서를 담은 PEM 파일.
  서버는 이 인증서를 자신의 CA로 검사한다.
- `sslkey`: 암호화하지 않은 개인 키를 담은 PEM 파일. Unix에서는 libpq와
  마찬가지로 다른 사용자가 읽을 수 없어야 한다. 권한은 `0600` 이하, 파일
  소유자가 root이면 `0640` 이하여야 한다.

두 파라미터를 함께 지정해야 한다. 인증서는 서버가 요청할 때 보낸다.

`sslcrl`과 `sslcrldir` 파라미터는 서버 인증서에 적용할 인증서 폐기 목록(CRL)을
지정한다.

- `sslcrl`: CRL 하나 이상을 담은 PEM 파일.
- `sslcrldir`: `openssl rehash`가 붙이는 이름(`<hash>.r0`, `<hash>.r1`, ...)의
  PEM 파일을 담은 디렉터리. 디렉터리의 다른 파일은 읽지 않는다.

두 파라미터를 함께 지정할 수 있다. libpq와 마찬가지로, 신뢰하는 CA 아래 체인의
모든 인증서에 그 발급자의 CRL이 있어야 하고, 다음 갱신 시각이 지난 CRL은
거절한다. CRL은 체인을 검사하는 경우(`verify-ca`, `verify-full`, 또는
`sslrootcert`에 파일을 지정한 경우)에 적용한다. 체인을 검사하지 않으면 CRL은
효과가 없으므로 설정 오류로 거절한다.

```sh
# A server that authenticates the client by certificate
v12n query mapping.ttl query.rq \
  --db 'postgresql://reader@db.internal/shop?sslmode=verify-full&sslrootcert=/etc/v12n/db-ca.pem&sslcert=/etc/v12n/reader.pem&sslkey=/etc/v12n/reader.key'
```

모든 파일은 연결을 열 때 읽는다.

서버가 `SCRAM-SHA-256-PLUS`를 제공하면 비밀번호 인증이 TLS 세션에 바인딩된다.
이를 제공하지 않는 서버를 거절하려면 `channel_binding=require`를 추가한다.

TLS 연결에 실패하면 데이터베이스 오류(종료 코드 3, kind `database`)가 된다.
오류 메시지는 원인을 명시한다. 예를 들어 인증서가 신뢰하는 CA로 이어지지 않으면
`invalid peer certificate: UnknownIssuer`를, 인증서에 호스트 이름이 없으면
`certificate not valid for name "..."`를, CRL에 인증서가 있으면
`invalid peer certificate: Revoked`를 출력한다. 파일을 읽을 수 없을 때도
데이터베이스 오류가 되며, 메시지에 그 파일을 명시한다.

설정은 연결 문자열에서만 읽는다. libpq의 환경 변수(`PGSSLMODE`,
`PGSSLROOTCERT`, `PGSSLCERT`, `PGSSLKEY`, `PGSSLCRL`, `PGSSLCRLDIR`, `PGHOST`
등)는 읽지 않으므로, `psql`을 위해 설정한 변수가 v12n의 연결을 바꾸지 않는다.
연결 문자열은 `v12n query`에서는 `--db`, `V12N_DATABASE_URL` 순으로,
`v12n serve`에서는 `V12N_DATABASE_URL`, `[database] url` 순으로 정한다. 같은
TLS 파라미터를 두 번 지정하면 마지막 값을 쓰고, 지정하지 않은 파라미터는 이
절에서 설명한 기본값을 쓴다.

libpq와 다른 점은 다음과 같다.

- `sslrootcert`를 지정하지 않으면 libpq는 `~/.postgresql/root.crt`를 읽는다.
  v12n은 대신 운영체제의 루트 인증서를 사용한다.
- `prefer`에서 TLS 핸드셰이크가 실패하면 libpq는 TLS 없이 다시 연결한다. v12n은
  오류를 보고한다.
- `sslcert`와 `sslkey`를 지정하지 않으면 libpq는 `~/.postgresql/postgresql.crt`와
  `~/.postgresql/postgresql.key`가 있을 때 이를 읽고, 없는 인증서 파일은 건너뛴다.
  v12n은 두 파라미터를 모두 지정했을 때에만 클라이언트 인증서를 보내고, 없는
  파일은 오류로 보고한다.
- 체인을 검사하지 않을 때 libpq는 `sslcrl`과 `sslcrldir`을 무시한다. v12n은
  이를 거절한다.
- 암호화한 개인 키(`sslpassword`)와 `PG*` 환경 변수는 지원하지 않는다.

## HTTP 엔드포인트

`v12n serve --config v12n.toml`은 `/sparql`에서 SPARQL 1.1 Protocol을 제공한다.
생존 확인(liveness check)용 `/healthz`도 제공한다.

요청은 다음 세 방식 중 하나로 쿼리를 전달한다.

- `GET /sparql?query=...`
- `application/x-www-form-urlencoded`(`query=...`)를 사용하는 `POST`
- `application/sparql-query`를 사용하고 쿼리를 본문으로 보내는 `POST`

`Accept` 헤더가 결과 형식을 정한다.

| 형식 | 미디어 타입 |
|---|---|
| SPARQL 1.1 Query Results JSON | `application/sparql-results+json` |
| SPARQL Query Results XML | `application/sparql-results+xml` |
| SPARQL 1.1 Query Results CSV | `text/csv` |
| SPARQL 1.1 Query Results TSV | `text/tab-separated-values` |

`q` 값이 가장 큰 형식을 선택한다. 각 형식은 그 형식과 일치하는 미디어 범위 중
가장 구체적인 범위의 `q` 값을 따른다. 따라서 `*/*`는 `text/csv;q=0`을 덮어쓰지
않는다. `q` 값이 같으면 표의 순서로 정한다. 따라서 `Accept`가 없거나 `*/*`인
요청은 JSON을, `text/*`인 요청은 CSV를 받는다. 이 형식 중 어느 것도 받지 않는
요청에는 `406`을 반환한다. CSV와 TSV 응답에는 `charset=utf-8`이 붙는다.

모든 형식의 결과는 PostgreSQL에서 행이 도착하는 대로 스트리밍된다.

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

상태 코드는 본문보다 먼저 전송된다. 따라서 statement timeout이나 XML이 담을 수
없는 값처럼 첫 행 이후에 발생한 오류는 상태 코드를 바꿀 수 없다. 이 경우 서버는
응답을 중단하고 오류를 로그에 기록한다. 응답은 완결되지 않은 채 끝나며, JSON과
XML은 닫히지 않은 상태로 남는다.

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

HTTP 클라이언트와의 TLS, 인증, CORS는 서버 앞단의 리버스 프록시가 담당한다.
데이터베이스와의 TLS는 데이터베이스 URL에서 설정한다([TLS](#tls) 참고).
