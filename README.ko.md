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

4. 데이터베이스를 멈추고 컨테이너와 네트워크를 지운다.

   ```sh
   docker compose -f testenv/docker-compose.yml down -v
   ```

같은 매핑과 쿼리로 `v12n explain`을 실행하면 DB 없이 생성된 SQL을 볼 수 있다.
테스트 환경은 [testenv/README.md](testenv/README.md)에 있다. 차등 테스트가 쓰는
Ontop 엔드포인트도 여기서 설명하며, 이 예제에는 필요하지 않다.

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

## 로드맵

지원 범위는 아래 순서대로 넓어진다. 각 단계는 하나의
[마일스톤](https://github.com/CharmingGroot/sparql-v12n/milestones)이며, 화살표는
뒤 단계가 앞 단계를 바탕으로 만들어진다는 뜻이다.

```mermaid
flowchart LR
  R1["1. 값 식"] --> R2["3. 연산자와 쿼리 형식"] --> R3["4. 집계"]
  M["2. 카탈로그 메타데이터"] --> O1["5. RDFS 온톨로지"] --> O2["7. OWL 2 QL 온톨로지"]
  M --> R4["6. 그래프 패턴과 경로"]
  O1 --> R4
  D["8. 더 많은 데이터베이스"]
```

1. 값 식: `BIND`, `SELECT` 안의 식, 산술, SPARQL 1.1 함수
2. 한 번 읽고 갱신하는 카탈로그 메타데이터, NOT NULL과 외래 키 제약을 이용한 최적화
3. `VALUES`, `MINUS`, `EXISTS`, 하위 쿼리, `ASK`, `CONSTRUCT`,
   `DESCRIBE`, 추가 결과 형식
4. `GROUP BY`와 집계
5. 온톨로지: RDFS entailment
6. 변수 술어, named graph, property path
7. 온톨로지: OWL 2 QL
8. PostgreSQL 이외의 데이터베이스

이 단계들 다음으로 AI 에이전트용 MCP 서버를 계획하고 있다.

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
