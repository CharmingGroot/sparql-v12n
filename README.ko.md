# sparql-v12n

[English](README.md) | 한국어 | [简体中文](README.zh-CN.md)

> 이 문서는 [README.md](README.md)(커밋 8c0e55a)의 번역이다. 내용이 다르면 영어 원문을 따른다.

<p align="center"><b>Rust로 작성한 가상 지식 그래프 엔진이다. R2RML 매핑에 따라 SPARQL 쿼리를 SQL로 변환해 원본 데이터베이스에서 실행하며, 데이터를 복사하지 않는다.</b></p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/how-it-works-dark.svg">
    <img alt="호출자가 v12n에 SPARQL 쿼리를 보낸다. R2RML 매핑은 쿼리 계획과 RDF term 구성에 사용되고, DB의 키와 컬럼 타입은 SQL 생성에 사용된다. PostgreSQL은 결과를 조회하는 SQL 쿼리 하나를 실행해 행을 반환한다. v12n은 행을 디코딩해 RDF term을 만들고, SELECT 바인딩을 JSON, XML, CSV, TSV로 스트리밍한다. 원본 데이터는 PostgreSQL에 남으며, 메타데이터 조회와 prepared statement 준비에 DB 왕복이 추가될 수 있다." src="docs/images/how-it-works-light.svg" width="760">
  </picture>
</p>

- 지원하는 SPARQL 쿼리를 R2RML 매핑에 따라 하나의 SQL 쿼리로 변환해 원본
  데이터베이스에서 실행한다. 원본 데이터를 별도 저장소에 복사하거나 적재할 필요가
  없다.
- 현재는 PostgreSQL을 지원한다. 다른 데이터베이스는 [로드맵](#로드맵)에 있다.
- 어떤 테이블과 컬럼이 어떤 클래스와 속성이 되는지는 R2RML 매핑이 정한다. 매핑에
  없는 데이터는 조회할 수 없다.
- 온톨로지에 따른 추론은 아직 지원하지 않는다. 클래스를 조회하면 매핑에 그
  클래스로 명시된 대상만 검색한다. [로드맵](#로드맵)을 참고한다.
- 실행 파일 `v12n` 하나가 명령줄 도구와 SPARQL 1.1 Protocol HTTP 엔드포인트를
  모두 제공한다.

> 상태: 실험 단계. 아래의 지원 범위는 Ontop 및 W3C R2RML 테스트 케이스와 대조해
> 검증한다. 1.0 이전에는 명령, 출력, API가 바뀔 수 있다.

## 목적

v12n은 관계형 데이터베이스에 있는 데이터를 별도 RDF 저장소에 적재하지 않고 RDF
그래프로 조회한다. 그래프는 R2RML 매핑이 정의한다. 각 쿼리는 원본 테이블에서
SQL로 실행되고, 결과에 포함된 행만 RDF term으로 변환된다. 대량의 `SELECT` 결과를
DB 행에서 직렬화된 응답까지 효율적으로 변환하고 반환하는 것이 이 프로젝트의 성능
목표다.

데이터가 PostgreSQL에 있고, 쿼리가 [지원 범위](#지원하는-sparql)에 들어가며,
온톨로지 추론 없이 매핑에 명시된 클래스와 속성만으로 충분할 때 이 엔진을 검토할
수 있다. 현재 실험 단계이며 PostgreSQL만 지원한다. Ontop과 비교한 벤치마크는
[성능](#성능)에서 연결한다.

## 설치

[릴리스](https://github.com/CharmingGroot/sparql-v12n/releases) 페이지에서 플랫폼에
맞는 압축 파일을 내려받고 `v12n`을 `PATH`에 둔다. 압축 파일은 Linux(x86_64,
aarch64, 정적 바이너리), macOS(Apple silicon), Windows(x86_64)용으로 빌드한다.

릴리스마다 `SHA256SUMS` 파일과 빌드 출처 attestation이 있다. 내려받은 파일은 아래
명령 중 하나로 확인한다(macOS에서는 첫 명령 대신 `shasum -a 256 -c`를 쓴다).

```sh
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify v12n-<version>-<target>.tar.gz -R CharmingGroot/sparql-v12n
```

Rust 1.88 이상으로 소스에서 빌드하려면 다음을 실행한다.

```sh
cargo install --locked --git https://github.com/CharmingGroot/sparql-v12n v12n-cli
```

`v12n --version`은 버전과 빌드한 커밋을 출력한다.

## 빠른 시작

`examples/shop`의 예제를 Docker로 실행한 로컬 PostgreSQL에서 실행한다. Compose v2가
있는 Docker와 `v12n` 실행 파일([설치](#설치) 참고)이 필요하고, 이 저장소를 clone한
디렉터리의 최상위에서 실행한다. 소스에서 빌드했다면 `v12n` 대신
`./target/release/v12n`을 쓴다.

```sh
git clone https://github.com/CharmingGroot/sparql-v12n.git
cd sparql-v12n
```

1. 예제 스키마와 데이터가 들어 있는 PostgreSQL을 시작한다. `testenv`의 `postgres`
   서비스만 시작되고, 로컬 개발용 자격 증명으로 `127.0.0.1:55432`에서 접속을 받는다.

   ```sh
   docker compose -f testenv/docker-compose.yml up -d --wait postgres
   ```

2. 매핑을 검사한다.

   ```sh
   v12n check examples/shop/mapping.ttl --format table
   ```

   ```text
   ok: 2 triples maps, 2 classes, 4 properties
   ```

3. VIP 고객이 낸 주문을 조회하는 쿼리를 실행한다.

   ```sh
   v12n query examples/shop/mapping.ttl examples/shop/queries/vip-orders.rq \
     --db postgresql://v12n:v12n-dev@127.0.0.1:55432/shop --format table
   ```

   ```text
   ?order                           ?amount
   <http://example.org/order/1001>  "120000"^^<http://www.w3.org/2001/XMLSchema#integer>
   <http://example.org/order/1003>  "87000"^^<http://www.w3.org/2001/XMLSchema#integer>
   <http://example.org/order/1004>  "15000"^^<http://www.w3.org/2001/XMLSchema#integer>
   ```

   이 세 주문이 반환되는 이유는 [결과가 만들어지는 과정](#결과가-만들어지는-과정)에
   있다.

4. 데이터베이스를 멈추고 컨테이너와 네트워크를 지운다.

   ```sh
   docker compose -f testenv/docker-compose.yml down -v
   ```

테스트 환경은 [testenv/README.md](testenv/README.md)에 있다. 차등 테스트가 쓰는
Ontop 엔드포인트도 여기서 설명하며, 이 예제에는 필요하지 않다.

### 결과가 만들어지는 과정

예제 데이터베이스에는 테이블이 두 개 있다. 아래는 두 테이블의 모든 행이다. 전체
파일은 [schema.sql](examples/shop/schema.sql)과
[data.sql](examples/shop/data.sql)이다.

`customers`

| id | name | tier |
|---|---|---|
| 7 | Kim | VIP |
| 8 | Lee | REGULAR |
| 9 | Park | VIP |

`orders`

| id | customer_id | amount |
|---|---|---|
| 1001 | 7 | 120000 |
| 1002 | 8 | 30000 |
| 1003 | 9 | 87000 |
| 1004 | 7 | 15000 |

R2RML 매핑은 각 행을 IRI와 속성으로 바꾼다. 아래 발췌에서는 prefix 선언,
`a rr:TriplesMap`, `ex:name`을 생략했다. 전체 파일은
[mapping.ttl](examples/shop/mapping.ttl)이다.

```turtle
<#CustomerMap>
  rr:logicalTable [ rr:tableName "customers" ] ;
  rr:subjectMap [ rr:template "http://example.org/customer/{id}" ; rr:class ex:Customer ] ;
  rr:predicateObjectMap [ rr:predicate ex:tier ; rr:objectMap [ rr:column "tier" ] ] .

<#OrderMap>
  rr:logicalTable [ rr:tableName "orders" ] ;
  rr:subjectMap [ rr:template "http://example.org/order/{id}" ; rr:class ex:Order ] ;
  rr:predicateObjectMap [ rr:predicate ex:amount ; rr:objectMap [ rr:column "amount" ] ] ;
  rr:predicateObjectMap [
    rr:predicate ex:placedBy ;
    rr:objectMap [ rr:template "http://example.org/customer/{customer_id}" ]
  ] .
```

- `id`가 7인 고객 행은 `<http://example.org/customer/7>`이 된다. 이 IRI는
  `ex:Customer`이고 `ex:tier "VIP"`를 가진다.
- `id`가 1001인 주문 행은 `<http://example.org/order/1001>`이 된다. 이 IRI는
  `ex:Order`이고 `ex:amount 120000`을 가진다. `integer` 컬럼이므로 값은
  `xsd:integer` 리터럴이다.
- `ex:placedBy`는 고객 subject와 같은 템플릿으로 `customer_id`에서 고객 IRI를
  만든다. 주문 1001의 `customer_id`는 7이므로 `<http://example.org/customer/7>`을
  가리키고, 이는 고객 행 7의 IRI와 같다.

쿼리 [vip-orders.rq](examples/shop/queries/vip-orders.rq):

```sparql
PREFIX ex: <http://example.org/>

# Orders placed by VIP customers.
SELECT ?order ?amount WHERE {
  ?customer a ex:Customer ; ex:tier "VIP" .
  ?order ex:placedBy ?customer ; ex:amount ?amount .
}
```

`?customer`가 두 패턴에 함께 나오므로, 주문의 `ex:placedBy` IRI가 등급이
`"VIP"`인 고객의 IRI와 같을 때만 그 주문이 결과에 들어간다. SQL에서는
`orders.customer_id = customers.id` 조인이 된다. VIP 고객은 7과 9이다. 고객 7의
주문은 1001과 1004이고, 고객 9의 주문은 1003이다. 주문 1002는 등급이
`"REGULAR"`인 고객 8의 주문이다.

`v12n explain`은 DB 없이 생성된 SQL을 출력한다. 조인은
`JOIN "orders" t1 ON t1."customer_id" = t0."id"`로 나타나고, `"VIP"`는
파라미터 `$1`로 전달된다.

```sh
v12n explain examples/shop/mapping.ttl examples/shop/queries/vip-orders.rq
```

## 지원하는 SPARQL

엔진은 R2RML 매핑 하나와 PostgreSQL 데이터베이스 하나를 대상으로 하는 `SELECT`
쿼리를 처리한다. 아래 표는 지원하는 기능과 거부하는 기능을 정리한다. 거부되는
쿼리나 표에 없는 형식의 쿼리는 오류로 끝나며(`v12n query`는 종료 코드 1, HTTP
엔드포인트는 501), 명세와 다른 답을 반환하지 않는다. 지원하는 기능은 모두 차등
테스트(`tests/diff`)에서 [Ontop](https://ontop-vkg.org/)의 결과와 비교한다.

| 기능 | 비고 | 거부 |
|---|---|---|
| 기본 그래프 패턴(basic graph pattern) | 상수 술어와 `rdf:type`. 상수와 공유 변수는 컬럼 타입이 달라도 RDF term으로 비교한다. 같은 트리플을 만드는 행이 여러 개여도 트리플은 한 번만 센다 | 변수 술어, property path, `GRAPH`, `FROM`, `FROM NAMED` |
| `SELECT`, `SELECT *`, `DISTINCT` | 해(solution)에 대한 bag 의미론 | `REDUCED`, 하위 쿼리, `GROUP BY`와 집계 |
| `BIND`, `SELECT (expr AS ?v)` | 변수, 상수, 정수와 decimal의 `+`, `-`, `*`, `/`. 오류가 나면 변수는 바인딩되지 않는다 | 그 밖의 식. `OPTIONAL`이나 `UNION` 안의 `BIND`. 계산한 변수를 트리플 패턴, `FILTER`, `ORDER BY`, 다른 식에서 쓰는 것 |
| `OPTIONAL` | 중첩·연속 사용, 내부의 `FILTER`, 바인딩되지 않은 변수 | 여러 `OPTIONAL` 부분에서만 바인딩되는 변수에 대한 `SELECT DISTINCT`, 한쪽이 빈 `UNION`을 감싸는 `OPTIONAL` 안의 `FILTER` |
| `UNION` | 양쪽의 bag 합집합(`OPTIONAL` 안에서도 지원). 한 변수에 형태가 다른 term map을 쓸 수 있다 | 같은 변수에서 자연 데이터타입 컬럼과 다른 리터럴 형태 사이의 중복 제거 |
| `FILTER` | `=`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `\|\|`, `!`, `BOUND`, `sameTerm`, `isIRI`, `isBlank`, `isLiteral`, `STR`, `LANG`, `DATATYPE`, `REGEX`, 정수와 decimal의 `+`, `-`, `*`, `/`. SPARQL의 오류 의미론을 따른다 | 그 밖의 함수, `xsd:double`의 산술, `IN`, `EXISTS`. SPARQL 1.1이 확장에 맡긴 비교(`xsd:date`의 순서 비교, 언어 태그가 있는 리터럴에 대한 `!=`, 시간대가 있는 dateTime과 없는 dateTime의 비교). `real` 컬럼에 대한 비교. `\d`, `\w`, `\p{..}`, 역참조를 쓰는 `REGEX` |
| `ORDER BY`, `LIMIT`, `OFFSET` | SPARQL 1.1의 순서. SPARQL이 정하지 않은 부분은 고정된 순서를 쓴다 | `SELECT DISTINCT`가 투영하지 않는 변수에 대한 `ORDER BY` |
| 리터럴(literal) | RDF 1.1 term 동등성. 언어 태그는 소문자로, `xsd:string`은 simple literal로 다루고, 값은 XSD 1.1 정규 표기(canonical form)로 쓴다 | `FILTER`에서 `xsd:float`와 파생 정수 타입을 값으로 비교하는 것 |
| 그 밖의 형식 | RDFS 추론 없음. `rdf:type`은 매핑에 명시된 클래스와만 일치한다 | `ASK`, `CONSTRUCT`, `DESCRIBE`, `MINUS`, `VALUES`, `SERVICE` |

표의 각 항목에 대한 규칙은 [docs/semantics.ko.md](docs/semantics.ko.md)에 있다.
[상수](docs/semantics.ko.md#쿼리-안의-상수), [조인](docs/semantics.ko.md#여러-컬럼이-공유하는-변수), [중복](docs/semantics.ko.md#중복-트리플), [`OPTIONAL`](docs/semantics.ko.md#optional),
[`UNION`](docs/semantics.ko.md#union), [`FILTER`](docs/semantics.ko.md#filter),
[`BIND`와 `SELECT` 식](docs/semantics.ko.md#bind와-select-식), [정렬](docs/semantics.ko.md#order-by-limit-offset), [리터럴](docs/semantics.ko.md#리터럴)을
참고한다. `tests/diff/README.md`에는 Ontop과의 알려진 차이와, 거부되는 테스트
쿼리마다 거부되는 이유가 정리되어 있다.

## 사용법

| 명령 | 용도 |
|---|---|
| `v12n check` | 매핑을 검증한다 |
| `v12n terms` | 매핑이 노출하는 클래스와 속성을 보여 준다 |
| `v12n explain` | DB 없이 쿼리의 SQL을 보여 준다 |
| `v12n query` | 쿼리를 실행하고 결과를 출력한다 |
| `v12n serve` | `/sparql`에서 SPARQL 1.1 Protocol을 제공한다 |

기본으로 켜져 있는 제한은 읽기 전용 DB 세션, 30초 실행 시간 제한, 해 10000개
제한이다. HTTP 엔드포인트는 쿼리 크기를 64 KiB로 제한하고, 동시에 최대
16개 요청을 처리한다.

각 명령의 출력 형식과 종료 코드, HTTP 엔드포인트와 그 설정은
[docs/usage.ko.md](docs/usage.ko.md)에 있다.

## 성능

[bench/README.md](bench/README.md)는 같은 PostgreSQL, 매핑, 쿼리로 Ontop과
비교하는 벤치마크를 설명하고, [bench/RESULTS.md](bench/RESULTS.md)는 측정 결과와
분석을 담는다. 각 엔진의 시간은 Ontop과 같은 해(solution)를 반환한 경우에만
집계한다.

아래 수치는 2026-10-26에 커밋 `57f7f63`에서 실행한 벤치마크 한 차례의 결과이며,
현재 main 브랜치의 측정값이 아니다. PostgreSQL 17.11에 `alerts` 1,000,000행,
`assets` 100,000행, `links` 200,000행, `observables` 2,200,000행을 적재했다.
v12n은 Apple M4(16 GiB)의 macOS에서 네이티브로 실행했고, Ontop 5.5.0(`-Xmx256m`)과
PostgreSQL은 Docker Desktop VM에서 실행했다. 각 시간은 10회 실행의 중앙값이며,
새 연결에서 SPARQL 1.1 Protocol 요청을 보낸 시점부터 JSON 응답이 끝날 때까지를
측정한다.

| 쿼리 | 결과 행 | v12n (ms) | Ontop (ms) |
|---|---:|---:|---:|
| `q06-union` | 145,000 | 124.5 | 1267.4 |
| `q07-order-limit` | 100 | 194.8 | 142.9 |

이 측정에서 v12n은 쿼리 9개 중 8개에서 Ontop보다 짧게 걸렸고, 정수와 IRI로
정렬하는 `q07-order-limit`에서는 더 오래 걸렸다. 이후의 변경으로 `alerts`
100,000행에서 이 쿼리의 시간이 줄었으며([#75 이후 IRI 정렬 키](bench/RESULTS.md#iri-sort-keys-after-75)),
`alerts` 1,000,000행 측정은 다시 실행하지 않았다. 이 결과는 이 기계와 이
쿼리들에 한정된다. Ontop의 힙은 256 MiB로 제한되었으며, 힙을 늘리면 큰 결과에서의 시간이
달라질 수 있다([주의 사항](bench/RESULTS.md#caveats)).

이 시간은 전체 소요 시간(end to end)이다. PostgreSQL의 SQL 실행, 행 디코딩,
RDF term 구성, 응답 작성이 모두 포함된다. SPARQL→SQL 변환 시간은 따로 측정하며,
보고서에서는 쿼리당 15–40 µs였다. 두 엔진 모두 데이터베이스 전체를 RDF로 변환하지
않으므로, 이 벤치마크는 전체 데이터의 RDF 물질화(materialization)를 측정하지
않는다.

## 로드맵

예정된 작업은 [마일스톤](https://github.com/CharmingGroot/sparql-v12n/milestones)으로
묶는다. 화살표는 뒤 마일스톤이 앞 마일스톤의 작업을 사용한다는 뜻이다. 경로로
이어지지 않은 마일스톤끼리는 서로 의존하지 않는다. 점선 상자는 main 브랜치에
반영된 작업이 아직 없는 마일스톤이다. 상태는 릴리스가 아니라 main 브랜치 기준이며,
현재 지원 범위와 제약은 [지원하는 SPARQL](#지원하는-sparql)에 있다.

```mermaid
flowchart LR
  M["M 카탈로그 메타데이터"] --> O1["O1 RDFS 온톨로지"] --> O2["O2 OWL 2 QL 온톨로지"]
  M --> R4["R4 그래프 패턴과 경로"]
  O1 --> R4
  M --> D["D 더 많은 데이터베이스"]
  R1["R1 값 식<br/>(일부 지원)"] --> R3["R3 집계"]
  R2["R2 연산자와 쿼리 형식<br/>(일부 지원)"]
  classDef planned stroke-dasharray: 5 4
  class R3,M,O1,O2,R4,D planned
```

| 마일스톤 | main에 반영된 것 | 남은 작업 |
|---|---|---|
| [R1 값 식](https://github.com/CharmingGroot/sparql-v12n/milestone/1) | `FILTER`의 산술, `BIND`와 `SELECT` 식 | 조건 함수, RDF term 함수, 문자열·수치·날짜와 시간·해시 함수, 형 변환, `ORDER BY`의 새 식 |
| [R2 연산자와 쿼리 형식](https://github.com/CharmingGroot/sparql-v12n/milestone/3) | SPARQL XML, CSV, TSV 결과 | `VALUES`, `MINUS`, `EXISTS`, 하위 쿼리, `ASK`, `CONSTRUCT`, `DESCRIBE` |
| [R3 집계](https://github.com/CharmingGroot/sparql-v12n/milestone/4) | | `GROUP BY`, `HAVING`, 집계 함수 |
| [M 카탈로그 메타데이터](https://github.com/CharmingGroot/sparql-v12n/milestone/2) | | 엔진과 백엔드 포트, 한 번 읽고 갱신하는 카탈로그 메타데이터, NOT NULL과 외래 키 제약을 이용한 최적화 |
| [O1 RDFS 온톨로지](https://github.com/CharmingGroot/sparql-v12n/milestone/5) | | RDFS entailment |
| [O2 OWL 2 QL 온톨로지](https://github.com/CharmingGroot/sparql-v12n/milestone/7) | | OWL 2 QL |
| [R4 그래프 패턴과 경로](https://github.com/CharmingGroot/sparql-v12n/milestone/6) | | 변수 술어, named graph, property path |
| [D 더 많은 데이터베이스](https://github.com/CharmingGroot/sparql-v12n/milestone/8) | PostgreSQL 전용 SQL 문법을 `Dialect` trait으로 분리 | PostgreSQL 외의 두 번째 데이터베이스 |

이 마일스톤들 다음으로 AI 에이전트용 MCP 서버를 계획하고 있다.

## 디렉터리 구성

| 경로 | 내용 |
|---|---|
| `crates/v12n-core` | SPARQL 파싱, 매핑 인덱스, unfold, 최적화, SQL 렌더링. 데이터베이스 의존성 없음 |
| `crates/v12n-postgres` | PostgreSQL에서의 실행과 키 메타데이터 조회 |
| `crates/v12n-server` | SPARQL 1.1 Protocol HTTP 엔드포인트 |
| `crates/v12n-cli` | `v12n` 명령 |
| `crates/v12n-difftest` | Ontop과 비교하는 차등 테스트. 케이스의 쿼리마다 테스트가 하나다 |
| `docs` | 사용법과 쿼리 의미 규칙 문서, README 그림 |
| `examples/shop` | 예제 스키마, 데이터, 매핑, 쿼리 |
| `bench` | 생성한 데이터로 Ontop과 비교하는 벤치마크와 그 결과 |
| `testenv` | 테스트용 로컬 PostgreSQL과 Ontop 기준 엔드포인트 |
| `tests/diff` | 차등 테스트 케이스(`examples/`도 케이스다), `tests/diff/w3c`의 W3C R2RML 케이스 |
| `tests/w3c/rdb2rdf` | W3C RDB2RDF 테스트 파일, 수정하지 않은 부분집합(W3C Document License) |

## 기여

변경을 제안하는 방법, 커밋 컨벤션, 테스트 실행 방법은 [CONTRIBUTING.md](CONTRIBUTING.md)를
참고한다.

## 라이선스

Apache-2.0. [LICENSE](LICENSE)를 참고한다.
