# 查询语义

[English](semantics.md) | [한국어](semantics.ko.md) | 简体中文

> 本文是 [semantics.md](semantics.md)（提交 8c0e55a）的翻译。如有出入，以英文原文为准。

本文介绍 sparql-v12n 如何将 SPARQL 查询翻译为 SQL，以及决定答案的规则：列名、IRI、字面量、常量、连接、去重、`OPTIONAL`、`UNION`、`FILTER` 与排序。总体介绍与支持功能表见 [README](../README.zh-CN.md)。

## 列名

`rr:column`、模板与连接条件中的列名是 SQL 标识符（R2RML §5）。其解析方式有以下两种：

- 定界名称（例如 `"\"Name\""`）按书写原样使用。
- 常规名称按 PostgreSQL 解析未加引号名称的方式解析。这会将名称折叠为小写。

例如，`rr:column "StudentId"` 读取 `SELECT ... AS StudentId` 产生的列，即 `studentid`。出于同样的原因，`rr:column "Name"` 不匹配以 `"Name"` 创建的列。

在执行使用常规名称的查询之前，引擎会准备一条探测语句，由 PostgreSQL 在其中解析该名称。然后，引擎在生成的 SQL 中为解析后的名称加引号。仅由小写字母、数字和 `_` 组成的名称无需探测。

## 相对 IRI

带有 `rr:termType rr:IRI` 的列或模板可能产生相对 IRI，例如 `Carlos` 或 `person/7`。R2RML 将相对 IRI 相对于提供给处理器的基础 IRI 进行解析（R2RML §4, §11.2）。

该解析是简单的字符串拼接，而不是 RFC 3986 解析。例如，以 `http://data.example.com/` 为基础 IRI 时：

- `Carlos` 变为 `<http://data.example.com/Carlos>`；
- `../x` 变为 `<http://data.example.com/../x>`。

模板值会先进行百分号编码。例如，`Bob/Charles` 得到 `Bob%2FCharles`。

不存在默认的基础 IRI。基础 IRI 通过 `base_iri`、`--base-iri` 或 `V12N_BASE_IRI` 设置。未设置时，相对 IRI 属于 R2RML 数据错误，错误信息会指出该设置项。这样，引擎从不为数据臆造 IRI。请将基础 IRI 设为数据所属的命名空间。它必须是绝对 IRI。

以 scheme（例如 `http:` 或 `urn:`）开头的字符串视为绝对 IRI。片段不会使字符串成为相对 IRI。

带有 scheme 但不是有效 IRI 的字符串属于数据错误。对于此类字符串，R2RML 会先尝试在其前面加上基础 IRI。两条规则仅在 `http://host:port/` 这类端口非数字的值上有所不同。引擎的规则使下文的 SQL 比较能够采用与结果相同的规则。

- 文本永远不会产生 scheme 的模板，例如 `{name}` 或 `person/{id}`，在加载映射时就在其前面加上基础 IRI。这样，`<http://data.example.com/person/7>` 这样的常量仍像以前一样在列上匹配（`id = $1`），并使用该列的索引。
- 列中可能存放相对或绝对两种形式中的任一种。因此，一个常量同时匹配两种形式，例如以 `http://data.example.com/a` 和 `a` 为参数的 `iri IN ($1, $2)`。该条件仍可使用该列上的索引。
- 在此类列上的连接会在 SQL 中比较生成的 IRI。在 `{scheme}:{rest}` 这类 scheme 取决于值的模板上的连接也是如此。该比较的形式为 `CASE WHEN iri ~ '^scheme:' THEN iri ELSE $base || iri END`。该列上的索引无法用于此比较。
- 去重按列值进行。因此，相对值与其解析得到的绝对 IRI（例如 `a` 与 `http://data.example.com/a`）仍计为两次。出于这一原因，设置了基础 IRI 时，对此类列使用 `SELECT DISTINCT` 会作为不支持的功能被拒绝。

作为对比，Ontop 5.5.0 会在每个不以绝对 IRI 开头的模板前加上 `http://example.com/base/`，即 W3C 测试用例的基础 IRI。Ontop 没有可更改该基础 IRI 的设置。对于来自列的相对 IRI，Ontop 也会失败。

## 列值的词法形式

引擎以规范形式写出每个非字符串值。规范形式即 XML Schema 1.1 Part 2 所定义的形式，R2RML 所引用的也是这一版本。该规则同时适用于字面量与模板值：

- `numeric` 12.50 与 3.00 分别变为 `"12.5"^^xsd:decimal` 与 `"3"^^xsd:decimal`。整数值的 decimal 不带小数点。
- `float8` 30 变为 `"3.0E1"^^xsd:double`。
- `boolean` 给出 `true`/`false`。
- `bytea` 给出大写十六进制。

因此，结果不依赖于列声明的标度。

Ontop 则写出 PostgreSQL 的文本输出（`12.50`、`"30"^^xsd:double`）。差分测试将这些记录为已知差异（issue #25）。

SQL `=` 判定为不同的两个值可能是同一个 term。PostgreSQL 将 `time '24:00:00'` 与 `'00:00:00'` 区分开。XSD 1.1 将两者映射到同一个值，写作 `00:00:00`。两个值都给出 `"00:00:00"^^xsd:time`，由此产生以下效果：

- 常量 `"00:00:00"^^xsd:time` 匹配两者。
- 常量 `"24:00:00"^^xsd:time` 不匹配任何值。
- 连接、`DISTINCT`、`UNION` 与 `ORDER BY` 将两者视为同一个值（`col + interval '0'`）。

`time` 列上的唯一键不会被用来证明查询没有重复。Ontop 按原样写出 `24:00:00`（issue #37）。

SQL `=` 判定为相等的两个值仍可能是不同的 term，例如以下情形：

- 在 `real` 与 `double precision` 列中，`-0` 和 `0` 分别给出 `"-0.0E0"` 与 `"0.0E0"`。
- `char(n)` 值保留其填充，而 `char(n)` 上的 `=` 会忽略填充。例如，`'a'::char(3)` 给出 `"a  "`。

引擎在常量、连接、`sameTerm` 以及 `DISTINCT` 与 `UNION` 所做的去重中，将此类列作为 term 比较。在这些位置，引擎还会比较值的文本（针对零）或其长度（针对 `char(n)`）。该比较与 `=` 并列进行，使该列上的索引仍能找到对应的行。

## 查询中的常量

三元组模式中的常量按 SPARQL 的要求作为 RDF term 匹配。词法形式与数据类型必须都相等。列值通过其 R2RML 自然映射参与比较。例如，`integer` 列给出规范形式的 `xsd:integer`，`numeric` 给出规范的 `xsd:decimal`，其他类型以此类推。

下表列出哪些常量匹配存放 7 的整数列：

| 常量 | 是否匹配 | 原因 |
|---|---|---|
| `7`、`<…/customer/7>` | 是 | |
| `"7"`、`"7"^^xsd:decimal`、`7.0` | 否 | 数据类型不同 |
| `"07"^^xsd:integer`、`<…/customer/007>`、`<…/customer/+7>` | 否 | 该常量不是映射产生的规范形式 |
| `99999999999`、`<…/customer/99999999999>` | 否 | 该值超出该列的取值范围，因此没有任何行包含该值 |

常量作为绑定参数发送给 PostgreSQL，从不出现在 SQL 文本中。

绑定之前，引擎会根据 PostgreSQL 从列推断出的参数类型检查每个常量。检查包括以下步骤：

1. 引擎将常量按该类型读取。
2. 引擎用构建结果 term 的同一段代码将该值格式化回去。
3. 仅当 round trip 完全一致时，引擎才发送该常量。类型允许时，以二进制形式发送。

无法匹配的常量会使其所在的 `UNION` 分支为空。引擎这样处理，而不是让 PostgreSQL 因无法解析的输入而失败。在 `OPTIONAL` 部分内部时，仅该部分不匹配，其左侧的解予以保留。没有剩余分支时，查询不访问数据库。

比较始终在列自身的类型上进行，因此会使用该列的索引。

部分列类型没有自然数据类型，例如 `interval`。对于这些类型，结果中携带转换为文本的列值。常量与同一文本进行比较（`col::text = $1`）。该比较无法使用该列上的索引。

## 字面量

字面量在所有位置都作为 RDF 1.1 term 进行比较。这包括三元组模式、连接、`sameTerm`、`FILTER` 中表示 RDFterm-equal 的 `=`、`DISTINCT` 以及结果。

- 语言标签比较时不区分大小写（RDF 1.1 Concepts §3.3）。因此，引擎以小写形式保存语言标签。例如，`rr:language "EN-us"` 给出 `"chat"@en-us`，常量 `"chat"@EN-US` 与之匹配。仅标签大小写不同的两个 term map 给出同一个 term。不是有效 BCP 47 语言标签的标签属于映射错误（R2RML §7.5）。`rr:datatype rdf:langString` 同样属于映射错误，因为 `rdf:langString` 仅是带标签字符串的数据类型。
- 简单字面量即 `xsd:string` 字面量。以下各项都给出同一个 term：`"x"`、`"x"^^xsd:string`、文本列、带有 `rr:datatype xsd:string` 的列以及字面量模板。结果中写出该 term 时不带数据类型。`DATATYPE` 返回 `xsd:string`。
- 没有任何 term map 产生的数据类型在三元组模式中永远不会匹配。例如，整数列给出 `xsd:integer` term，因此 `"5"^^xsd:int` 与 `"5"^^xsd:decimal` 在该处不匹配任何值。自定义数据类型在该处同样不匹配任何值。在 `FILTER` 中，数字按值比较，`xsd:float` 与派生整数类型会被拒绝（见 `FILTER`）。
- `char(n)` 值在 RDF term 中保留尾部的填充空格。SQL 标准与 W3C RDB2RDF 预期输出均如此（D018：`"Venus          "`）。如需不带填充的值，请使用 `varchar` 或 `text`，或使用带 `rtrim()` 的 R2RML 视图。

跨数据类型的值比较属于 `FILTER` 的范畴（见下文）。例如，对整数列使用 `FILTER(?amount = 7.0)`。

## 跨列共享的变量

出现两次的变量即为连接。连接同样比较 RDF term，而不是 SQL 值。

在执行此类查询之前，引擎会向 PostgreSQL 查询相关列的类型。引擎通过准备一条探测语句完成这一查询。然后，引擎执行以下处理：

- 用 `=` 比较同类的列（整数、文本、日期等），从而使用索引。
- 对其他列对，通过在 SQL 中计算的自然词法形式进行比较（`int_col::text = text_col::text`）。引擎会先检查自然数据类型。例如，存放 7 的 `integer` 列仅当文本列带有 `rr:datatype xsd:integer` 时才与其连接。它从不与 `numeric` 7.0 连接。`numeric` 值的规范形式以 `trim_scale(col)::text` 计算（PostgreSQL 13 或更高版本）。有测试确认该形式与引擎自身的格式化结果一致。
- 当模板或 IRI 列只能作为字符串比较时，像 IRI 那样对模板值进行百分号编码（R2RML 的 IRI-safe 形式）。
- 当词法形式无法在 SQL 中计算时，将查询作为不支持的功能拒绝（退出码 1，HTTP 501）。例如 `float8` 或日期/时间模板值与文本列比较。引擎拒绝此类查询，而不是在 PostgreSQL 中失败或返回错误的行。

## 取自不同类型列的变量

同一模式的各个 term map 可以从不同 SQL 类型的列读取同一个变量。例如，该变量可以来自一个 triples map 中的 `integer` 列和另一个 triples map 中的 `text` 列。

`UNION` 要求每个结果列只有一种类型。因此，对于涉及多个 term map 的查询，引擎还会向 PostgreSQL 查询所选列的类型。然后，引擎执行以下处理：

- 当类型可以合并且不改变任何值时，保留一个结果列。例如 `int4` 与 `int8`，以及 `text` 与 `varchar`。
- 将自然数据类型的字面量拆分为每种数据类型一个结果列。每个分支填充自己的列，其余列保持 `NULL`。这样，整数 120 保持为 `"120"^^xsd:integer`，文本 `'120'` 保持为 `"120"`。
- 当两种类型可能给出同一个 term 时，选取按连接中同样方式在 SQL 中计算的自然词法形式。这样，来自两个 term map 的同一个三元组仍只计一次。这适用于以下 term：
  - 模板值（基于 `integer` 键与基于 `text` 键的 `item/{id}` 都给出 `item/1`）；
  - IRI 列；
  - 带 `rr:datatype` 或语言标签的字面量；
  - 来自 `text` 与 `uuid` 列的普通字面量。
- 当该形式无法在 SQL 中计算时，将查询作为不支持的功能拒绝。这适用于 `float8`、`real` 与 `double precision` 的组合、日期/时间以及 `char(n)`。

## 重复的三元组

RDF 图是三元组的集合。因此，一个三元组无论由多少行产生都只计一次。在以下情形中，多行可能产生同一个三元组：

- 无键表中的重复行；
- 重复输出行的 R2RML 视图；
- 产生同一三元组的两个 term map。

基本图模式对其全部变量的每个不同绑定给出一个解。在此之后，`SELECT` 才进行投影。因此，`SELECT ?city WHERE { ?p ex:city ?city }` 仍会为每个人返回一次城市。

生成的 SQL 使用 `DISTINCT`（或跨 term map 使用 `UNION`）去重。去重针对模式中每个变量的列进行。当其中部分变量未被选取时，去重在子查询中进行。

部分查询通过单一 term map 组合（在 `UNION` 的某一侧中）读取表（而非视图）。在执行此类查询之前，引擎会从系统目录读取这些表的唯一键。当每个表在解所确定的列中都有一个键时，任意两行都不可能给出相同的解。此时，SQL 保留所有行而不使用 `DISTINCT`。

否则，PostgreSQL 必须去重。这需要对行进行哈希或排序。除非有索引按顺序提供这些行，否则 PostgreSQL 还会在返回第一行之前读取全部行。

`v12n explain` 没有数据库，因此始终显示 `DISTINCT`。

重复按列值比较。因此，模板渲染为相同字符串的两个不同值仍计为两次。例如，模板 `{a}-{b}` 将值 `x-`、`y` 与值 `x`、`-y` 渲染为同一字符串。

未被选取的变量按其列的类型与文本输出比较。因此，相等的 term 在以下情形中仍可能计为两次：

- 不同类型的列，且 SQL 无法计算其词法形式；
- 类型可以合并的列，例如 `varchar` 与 `text`；
- 同一类型但文本输出不同的值，例如 `numeric` 1.0 与 1.00。

## 来自多个逻辑表的主语

不同逻辑表上的两个 triples map 可能使用能产生同一 IRI 的主语模板，例如表 `src0` 和 `src1` 上都使用 `http://example.org/person/{id}`。一个 IRI 指代一个 RDF 节点，因此两个表的行描述相同的主语。映射以这种方式合并数据源。生成的 SQL 按模板列连接两个表，例如 `JOIN "src1" t1 ON t1."id" = t0."id"`。

这种方式有以下后果：

- 不同表中模板列取值相同的行成为同一个主语。如果两个数据源各自独立地为记录编号，无关的记录会被合并。
- 主语拥有每个数据源中某个属性的全部值。基本图模式返回这些值的全部组合。若四个属性在两个数据源中的值各不相同，一个主语有 2⁴ = 16 个解。若两个数据源的值相同，两行产生同一个三元组，该三元组只计一次（见[重复的三元组](#重复的三元组)）。
- 两个 triples map 都能匹配的三元组模式有两个候选。因此，由 n 个此类三元组模式组成的基本图模式展开为 2ⁿ 个 SQL 分支。分支超过 64 个的查询被拒绝。

`v12n check` 将每一对此类 triples map 报告为警告。警告不改变退出码。固定文本不同的两个模板（例如 `http://example.org/person/{id}` 与 `http://example.org/order/{id}`）不会产生同一 IRI，因此不报告。

## OPTIONAL

`OPTIONAL` 遵循 SPARQL 代数的 LeftJoin。左侧的每个解都会保留。当可选部分存在兼容解时，左侧的解由其中每个兼容解进行扩展。SQL 中每个可选部分对应一个 `LEFT JOIN`。

部分也可以改为读取左侧的一行：其唯一的表是左侧的表，通过 `NOT NULL` 唯一键连接，且其余条件都是列之间的比较。此时部分只能匹配那一行，因此它的每一列在条件成立处是该行的列，其余处为 `NULL`（`CASE WHEN … THEN … END`）。部分中有 `FILTER` 或常量时保留 `LEFT JOIN`。`v12n explain` 没有键，始终显示 `LEFT JOIN`。

- 一个部分的表先彼此连接。有多个表时，它们置于括号内。`ON` 子句包含该部分自身的条件与到左侧的关联。
- 嵌套的 `OPTIONAL` 是外层 `LEFT JOIN` 内部的 `LEFT JOIN`。写在 `OPTIONAL` 之后的模式在其之后连接。
- 共享变量与连接中一样按 RDF term 比较。因此，带类型的列与模板分解的处理方式相同。两侧均为普通列时会使用索引。
- 未绑定变量在 SQL 中为 `NULL`，在 JSON 绑定中省略。
- 共享变量可能在某一侧未绑定。当它仅由更早的 `OPTIONAL` 绑定，或仅由嵌套在该部分中的 `OPTIONAL` 绑定时，即属于这种情况。此类变量与任何值兼容。此时条件为 `(left IS NULL OR left = right)`，该列上的索引无法用于此条件。
- 每个部分先按代数的求值方式单独翻译。因此，将嵌套部分与外层左侧关联的条件在外层部分的 `ON` 子句中检查。该条件从不在嵌套部分内部检查。
- 仅由 `OPTIONAL` 部分绑定的变量从每个这样的部分获得列。其值为该行中第一个已绑定的值。
- 可选部分中的常量 term 附带一个标志列。该标志表示该部分是否匹配。
- 与任何基本图模式一样，重复的三元组在每个部分内只计一次。

可选部分可能以多种方式匹配。当多种 triples map 组合可以匹配它，或其内部 `UNION` 的多个侧可以匹配时，即属于这种情况。此时，该部分是这些选择的并集，代数给出以下恒等式。

LeftJoin(L, R1 ∪ R2) 为 Join(L, R1) ∪ Join(L, R2) ∪ Diff(L, R1 ∪ R2)。

SQL 分支随之拆分：

- 每个选择对应一个分支。该分支带有该选择的 `LEFT JOIN`，以及要求其部分已匹配的 `WHERE` 条件。PostgreSQL 会将其转换为内连接。
- 另有一个分支，带有所有选择的 `LEFT JOIN`，以及要求均未匹配的 `WHERE` 条件。该分支是反连接。它保留没有任何解可扩展的左侧解。

每个部分保留其 `ON` 子句。因此，与单个部分的情况一样，由兼容性和 `OPTIONAL` 内的 `FILTER` 决定部分是否匹配。在外层部分内部，这些拆分成为外层部分的选择。一侧为空的 `UNION` 通过该空侧原样扩展每个左侧解。

以下情形尚不支持，作为不支持的功能拒绝：

- 对仅由多个 `OPTIONAL` 部分绑定的变量使用 `SELECT DISTINCT`；
- `OPTIONAL` 内部包围一侧为空的 `UNION` 的 `FILTER`。

## UNION

`{ A } UNION { B }` 以 bag 形式包含两侧的解。两侧都给出的解计为两次。在 `SELECT DISTINCT` 下，该解计为一次。仅由一侧绑定的变量在另一侧的解中未绑定。

- 连接、`FILTER` 与 `OPTIONAL` 对位于其左侧的并集满足分配律。**arm** 即对每个 `UNION` 各选一侧所得的组合。基于这一分配律，引擎单独展开每个 arm，再以 `UNION ALL` 合并各 arm。
- 每个 arm 与任何其他模式一样翻译。重复的三元组在一个 arm 内部只计一次。arm 使用 `DISTINCT`，或跨其 term map 使用 `UNION`，或在键能证明不存在重复时不做处理。键按每个 arm 分别检查。各 arm 之间从不相互比较。
- 每个 arm 的每种 term map 组合对应一个 SQL 分支。所有 arm 的分支合计上限为 64，与单个模式的分支上限相同。
- 一侧中的 `FILTER` 仅作用于该侧的解。包围该并集的组中的 `FILTER` 作用于两侧的解。在这些解中，另一侧的变量未绑定。
- `OPTIONAL` 内部的 `UNION` 不对左连接满足分配律。其各侧是可选部分的选择（见 `OPTIONAL`）。每侧的解保留在各自的 arm 中。

一个变量可以从不同形态的 term map 取值。形态可能在并集的不同侧之间或不同 term map 之间有所不同：

- 一处为模板，另一处为列或另一个模板；
- 常量、IRI 与字面量；
- 不同数据类型或语言标签的字面量。

结果根据产生该行的分支的 term map 重建每个值。分为两种情况。

- 永远不可能相同的形态的 term 获得各自的结果列。每个分支填充自己的列。基于这些列的去重保持精确。此类形态包括：
  - `http://example.org/customer/{id}` 与 `http://example.org/wholesale/{code}` 这样的两个模板，其固定文本不同，或一方的文本在另一方的值中总会被 IRI-safe 编码转义；
  - IRI 与字面量；
  - 两个不同的常量；
  - 两个语言标签。
- 可能给出同一 term 的形态，在需要比较其重复时（在一个 arm 内或在 `SELECT DISTINCT` 下）共享一个结果列。该列包含 term 的字符串。SQL 与连接中一样计算该字符串（IRI-safe 模板、相对于基础 IRI 解析的 IRI、自然词法形式）。因此，来自模板和来自 IRI 列的同一三元组只计一次。此类列无法使用索引，并且要为每一行计算该字符串。在以下两种情形中，这种情况作为不支持的功能被拒绝：
  - SQL 无法计算词法形式时，与连接相同；
  - 其中一种形态是自然数据类型列的字面量，该列的数据类型取决于 SQL 类型。

## FILTER

`FILTER` 遵循 SPARQL 的求值规则，包括错误的处理。错误既不是真也不是假。`error || true` 为真，`error && false` 为假，`!error` 仍为错误。

SQL 以 `NULL` 表示错误。SQL 的 `AND`、`OR` 与 `NOT` 对 `NULL` 的处理方式相同。`WHERE` 仅保留过滤条件为真的行。SPARQL 判定为错误的比较在 SQL 中为 `NULL`，从不为 `false`。因此，`FILTER(!(?x = 5))` 会丢弃 `?x` 未绑定或为字符串的行。

过滤条件的作用范围：

- 组中的 `FILTER` 作用于整个组，无论写在组中何处。它只能看到该组的变量。例如，在 `{ ?s ex:name ?n { ?s ex:qty ?q FILTER(?n = "x") } }` 中，该过滤条件为错误，因为 `?n` 在嵌套组内未绑定。
- `OPTIONAL` 内的 `FILTER` 进入该部分 `LEFT JOIN` 的 `ON` 子句。它决定该部分是否匹配。它从不决定保留左侧的哪些行。它可以看到左侧的变量。当其为错误时，该部分不匹配，与 SQL 的 `LEFT JOIN` 相同。
- 位于 `OPTIONAL` 之后的 `FILTER` 处于 `WHERE` 中，会丢弃行。
- 除 `BOUND` 外，未绑定变量对所有函数都是错误。
- 仅由 `OPTIONAL` 部分绑定的变量从已匹配的部分读取（`CASE WHEN part matched THEN ... END`）。

比较遵循 SPARQL 的运算符映射。

| 操作数 | `=`、`!=`、`<`、`<=`、`>`、`>=` |
|---|---|
| 数字（`xsd:integer`、`xsd:decimal`、`xsd:double`） | 按值比较，采用 XPath 类型提升。`7.0` 等于整数 7，`-0` 等于 `0`，NaN 不等于任何值。 |
| 字符串（简单字面量、`xsd:string`） | 按码位比较（`COLLATE "C"`，要求数据库使用 UTF-8）。 |
| `xsd:boolean` | `false` < `true`。 |
| `xsd:dateTime` | 两个值都带时区或都不带时区时按时刻比较。带时区的值与不带时区的值之间的比较依赖隐式时区，因此被拒绝。 |
| IRI、空白节点以及其他类型的字面量 | 仅支持 `=` 与 `!=`，作为 RDF term 比较（RDFterm-equal）。`<` 及其他运算符为错误。两个不同字面量之间的比较为错误。 |

支持整数、decimal 与 double 的算术（`+`、`-`、`*`、`/` 与一元负号）。结果类型遵循 XPath 的类型提升：整数除以整数得到 decimal（`7 / 2` 为 `3.5`），带有 `xsd:double` 操作数的运算得到 double。SQL 以 `numeric` 计算整数与 decimal，因此大整数之和不会使 `int8` 溢出。整数与 decimal 的除以零、`numeric` 的 NaN 与无穷大，以及不是数字的操作数都是错误。对算术结果使用 `STR`、`LANG`、`DATATYPE` 或 `sameTerm`，目前作为不支持的功能被拒绝（Discussion #144）。

double 按 XPath 的要求遵循 IEEE 754：

- 除以零时，按两个操作数的符号（包括零的符号）得到 `INF` 或 `-INF`，`0e0 / 0` 得到 `NaN`。
- 对 double 而言过大的结果为无穷大，过小的结果为带其符号的零。
- NaN 不等于任何值，也不与任何值排序，因此对 NaN 而言 `!(?x * 2 > 0)` 成立。

PostgreSQL 的 `float8` 运算符在这些情形下会报错，因此每个运算符的 SQL 先用精确的检查判定这些情形。PostgreSQL 测试在边界值、边界附近的操作数对以及随机操作数上，将每个运算符与 IEEE 754 对照。整数或 decimal 操作数变为最接近的 double。绝对值不小于 2^1024 − 2^970 的 decimal 变为无穷大，不大于 2^-1075 的 decimal 变为带其符号的零（XML Schema 1.1，`floatingPointRound`）。仅由常量构成的算术在发送查询之前计算。

与列比较的常量，在能给出相同结果的情况下转换为列的类型。此时可以使用该列上的索引。例如，`?qty > 5` 即 `qty > $1::int8`，`?qty = 7.0` 即 `qty = $1::int8`。

否则，将列转换为 XPath 类型提升后的类型，该列上的索引无法用于此比较。例如，`?qty < 5.5` 将整数列转换为 `numeric`，`?qty < 5.5e0` 将其转换为 `float8`。

支持 `sameTerm`、`BOUND`、`isIRI`、`isBlank`、`isLiteral`、`STR`、`LANG` 与 `DATATYPE`。在需要布尔值的位置，term 代表其有效布尔值。空字符串与零为假。

`IF`、`COALESCE`、`IN` 与 `NOT IN` 遵循 SPARQL 1.1 §17.4.1：

- 当 `cond` 的有效布尔值为真时，`IF(cond, a, b)` 为 `a`，为假时为 `b`。`cond` 为错误时，`IF` 为错误。只对被选中的参数求值，因此另一个参数中的错误不影响结果。
- `COALESCE(e1, e2, ...)` 为第一个不是错误的参数。未绑定变量、除以零以及对字符串的算术都是错误，`COALESCE` 跳过这些参数。所有参数都是错误时，`COALESCE` 为错误。
- `lhs IN (e1, e2, ...)` 即 `(lhs = e1) || (lhs = e2) || ...`，`NOT IN` 是其否定。除非另一个比较为真，单个比较中的错误使结果为错误。因此当 `?x` 为 `"b"` 时，`?x NOT IN ("a", 1)` 为错误。列表为空时，`IN` 为假，`NOT IN` 为真。

在 SQL 中，`IF` 与 `COALESCE` 是每个参数对应一个分支的 `CASE` 表达式。`COALESCE` 中对算术的分支还要求值不为 `NULL`。`IN` 是各比较的 `OR`，每个比较的写法与 `=` 相同，因此 `?qty IN (5, 7)` 即 `qty = $1::int8 OR qty = $2::int8`，可以使用该列上的索引。

`ABS`、`ROUND`、`CEIL` 与 `FLOOR`（SPARQL 1.1 §17.4.4）遵循 `fn:abs`、`fn:round`、`fn:ceiling` 与 `fn:floor`（XPath F&O 3.1 §4.4）。结果的数据类型与参数相同：整数的结果为整数，decimal 的结果为 decimal，double 的结果为 double。不是数字的参数为错误，`numeric` 的 NaN 与无穷值也是错误。`ROUND` 将 .5 向正无穷方向舍入：`ROUND(2.5)` 为 `3`，`ROUND(-2.5)` 为 `-2`，`ROUND(-0.5e0)` 为 `-0.0E0`。

在 SQL 中，整数的舍入、向上取整与向下取整都是其自身。decimal 以 `numeric` 计算。PostgreSQL 的 `round(numeric)` 将 .5 向远离零的方向舍入，因此 decimal 的 `ROUND` 为 `floor(x + 0.5)`。对于 double，`float8` 的 `abs`、`ceil` 与 `floor` 按 XPath 的要求保留 NaN、无穷值与零的符号。PostgreSQL 的 `round(float8)` 将 .5 向偶数舍入，而 `floor(x + 0.5)` 在和被舍入时出错，例如 0.49999999999999994。因此 double 的 `ROUND` 是对每个 double 都精确的 `CASE` 表达式：NaN、无穷值以及绝对值不小于 2^52 的值舍入为其自身，-0.5 到 -0 之间的值得到 -0，其余的值将 `x - floor(x)` 与 0.5 比较。

`RAND` 作为不支持的功能拒绝。其值在每一行、每次执行中都不同。

`YEAR`、`MONTH`、`DAY`、`HOURS`、`MINUTES`、`SECONDS`、`TIMEZONE` 与 `TZ`（SPARQL 1.1 §17.4.5）接受来自 `timestamp` 或 `timestamptz` 列、常量或 `NOW` 的 `xsd:dateTime`，并给出其在自身时区中的组成部分（XPath F&O 3.1 §9.5）。`YEAR` 至 `MINUTES` 给出 `xsd:integer`，`SECONDS` 给出带小数部分的 `xsd:decimal`，例如 `58.25`。`TIMEZONE` 给出 `xsd:dayTimeDuration`，例如 `-PT5H30M`，对不带时区的值为错误。`TZ` 给出书写的时区，例如 `-05:30` 或 `Z`，对不带时区的值为 `""`。不是 `xsd:dateTime` 的参数为错误，作为普通字面量的无穷 `timestamp` 与 `timestamptz` 值也是错误。

这些函数与引擎写出的词法形式一致。`timestamp` 没有时区：其 `TZ` 为 `""`，`TIMEZONE` 为错误。`timestamptz` 以 UTC 加 `Z` 写出，因此无论会话时区如何，其组成部分均为 UTC 下的值，`TZ` 为 `Z`，`TIMEZONE` 为 `PT0S`。存入 `timestamptz` 列的 `2025-01-01T03:00:00+05:00` 写作 `2024-12-31T22:00:00Z`，其 `YEAR` 为 `2024`。在 SQL 中，组成部分是对列的 `extract`，`timestamptz` 以 `AT TIME ZONE 'UTC'` 读取，列为无穷值时为 `NULL`。PostgreSQL 将公元 1 年之前的一年记为 -1 年，而引擎将其写作 `0000`，因此 `YEAR` 对 1 年之前的年份加 1。常量在发送查询之前计算：`HOURS("2024-12-31T22:59:00-05:00"^^xsd:dateTime)` 为 `22`，其 `TZ` 为 `-05:00`，`24:00:00` 为次日的第一个时刻。

`NOW`（SPARQL 1.1 §17.4.5.1）是整个查询中唯一的 UTC `xsd:dateTime`，精确到微秒，在渲染查询时取得，并作为一个 `timestamptz` 参数发送。查询中的每个 `NOW` 都是该值，因此 `?t < NOW()` 将每一行与同一时刻比较。其 `TZ` 为 `Z`。

对来自 `date` 与 `time` 列或常量的 `xsd:date` 与 `xsd:time` 值，`YEAR` 等函数作为不支持的功能拒绝。SPARQL 1.1 仅为 `xsd:dateTime` 定义这些函数，而扩展它们的引擎结果各不相同。`TIMEZONE` 的结果与 `xsd:dayTimeDuration` 常量的比较与运算符映射之外的其他数据类型一样被拒绝；`STR(TIMEZONE(?t)) = "PT0S"` 比较其词法形式。

列的 `STR` 与 `DATATYPE` 在其词法形式或自然数据类型可在 SQL 中确定时可用，与连接相同。`date` 与 `timestamp` 的无穷值是普通字面量，因此其 `DATATYPE` 为 `xsd:string`。

`REGEX` 将 XPath 正则表达式翻译为匹配结果相同的 PostgreSQL 模式。该模式在 `COLLATE "C"` 下以 `~` 应用。

| XPath | PostgreSQL 模式 |
|---|---|
| `.`（不带 `s`） | 除换行符与回车符之外的任意字符，`[^\u000A\u000D]`。 |
| `^`、`$` | 字符串的开头与结尾，即 PostgreSQL 的默认行为。 |
| 带 `m` 的 `^`、`$` | 行边界，但不在末尾的换行符之后（XPath 3.1），使用环视实现。 |
| `i` | 每个字符展开为其大小写变体（`lower-case` 或 `upper-case` 相等的字符）。因此，无论数据库的区域设置如何，`i` 也匹配 `ı`，`k` 也匹配开尔文符号。 |
| `\s` | 恰好为空格、制表符、换行符与回车符。 |
| `x` | 移除字符类之外的空白。 |
| 其他字符 | 除 ASCII 字母或数字外，写作 `\uXXXX` 转义。 |

以下情形作为不支持的功能拒绝：

- `\d`、`\w`、`\i`、`\c`、`\p{..}` 及其补集，即 PostgreSQL 仅通过区域设置提供的 Unicode 类；
- 反向引用；
- 字符类减法；
- 大于 255 的计数；
- 字符类内部的 `\S`；
- `q` 标志；
- 非常量的模式或标志。

无效的模式或标志为错误，与 XPath 相同。

字符串函数 `STRLEN`、`SUBSTR`、`UCASE`、`LCASE`、`STRSTARTS`、`STRENDS`、`CONTAINS`、`STRBEFORE`、`STRAFTER`、`ENCODE_FOR_URI`、`CONCAT` 与 `REPLACE` 遵循 SPARQL 1.1 §17.4.3：

- 参数为字符串字面量，即简单字面量、`xsd:string` 与带语言标签的字面量（§17.4.3.1.1）。其他 term（例如数值、IRI 或未绑定变量）使函数成为错误。
- `STRSTARTS`、`STRENDS`、`CONTAINS`、`STRBEFORE` 与 `STRAFTER` 的参数必须兼容（§17.4.3.1.2）：两者都没有语言标签、两者标签相同，或仅第一个参数有标签。
- `SUBSTR`、`UCASE`、`LCASE` 与 `REPLACE` 保留第一个参数的语言标签。所有参数具有相同语言标签时，`CONCAT` 保留该标签，否则给出简单字面量（§17.4.3.12）。`ENCODE_FOR_URI` 给出简单字面量。
- `STRBEFORE` 与 `STRAFTER` 给出第二个字符串首次出现位置之前与之后的字符，并带有第一个参数的语言标签；第二个字符串不出现时给出简单字面量 `""`（§17.4.3.9、§17.4.3.10）。
- `ENCODE_FOR_URI` 将 `A-Z`、`a-z`、`0-9`、`-`、`_`、`.` 与 `~` 以外的每个字符按 UTF-8 字节写成大写的 `%XX`（F&O 3.1 §6.2）。
- `REPLACE(arg, pattern, replacement, flags)` 与 `fn:replace` 相同地替换模式的每个匹配（F&O 3.1 §5.6.4）。模式、替换字符串与标志为简单字面量常量，模式与 `REGEX` 一样翻译。替换字符串中 `$N` 为第 `N` 个分组，`$0` 为整个匹配，`\$` 与 `\\` 为 `$` 与 `\`。匹配空字符串的模式（例如 `a*`）以及含其他 `$` 或 `\` 的替换字符串为错误。
- 长度与位置按字符（Unicode 码位）计算：`"e\u0301"` 的 `STRLEN` 为 2，一个 emoji 的 `STRLEN` 为 1。`SUBSTR(s, start, length)` 与 XPath 的 `fn:substring` 相同，取从 1 开始计数、满足 `start <= p < start + length` 的位置 `p` 上的字符。起始位置与长度为 `xsd:integer` 常量。
- `UCASE` 与 `LCASE` 采用不带语言定制的 Unicode 完整大小写映射（XPath F&O 3.1 §5.4.7、§5.4.8）：`UCASE("Straße")` 为 `"STRASSE"`，`LCASE("ΟΔΟΣ")` 为 `"οδος"`。

在 SQL 中，所有计算得到的字符串都在 `COLLATE "C"` 下，因此其比较、查找与连接按码位进行，与列的 collation 无关。`STRLEN` 为 `char_length`，`SUBSTR` 为 `substr`，在 UTF-8 数据库中按码位计数。`UCASE` 与 `LCASE` 为 ICU collation `"und-x-icu"`（root 区域设置）下的 `upper` 与 `lower`，未启用 ICU 构建的 PostgreSQL 服务器会拒绝该语句。与常量比较的 `STRSTARTS`、`STRENDS` 与 `CONTAINS` 为 `LIKE`，常量作为参数传递，其中的 `%`、`_` 与 `\` 被转义；列上 `COLLATE "C"` 的索引或使用 `text_pattern_ops` 的索引可用于 `STRSTARTS`。`STRBEFORE` 与 `STRAFTER` 使用 `strpos` 与 `substr`；`REPLACE` 为带 `g` 标志的 `regexp_replace`，翻译后的模式与替换字符串作为参数传递。XPath 在同一位置有多个匹配时选择第一个分支并依次让贪婪量词尽量多取，PostgreSQL 则选择整个模式的最长匹配，因此 `REPLACE` 只接受两者选择相同匹配的模式：不含 `|`、非贪婪量词与作用于分组的量词，且每个重复的字符集合之后只能出现其不包含的字符。`STRLEN` 为数值，可以作为 `BIND` 与 `SELECT` 表达式的值；其他函数给出字符串，计算得到的变量目前不保存字符串。

以下情形作为不支持的功能拒绝，而不是给出与扩展了 SPARQL 的引擎不同的答案：

- 当数据类型不在上表中时（例如 `xsd:date`），对该数据类型的两个字面量进行排序或比较；
- 对两个带语言标签的字面量进行排序或比较，否定之外的 `=` 除外；
- 涉及以下任一项的比较与算术：
  - `real` 列，其 `xsd:double` 字面量的值是其最短十进制形式的值；
  - `rr:datatype` 为该列自然数据类型以外的数值、布尔或 dateTime 数据类型的列的值；
  - `xsd:float` 与派生整数类型；
  - 超出公元 1 至 9999 年范围或精度细于微秒的 dateTime 常量；
- 包括 `RAND` 在内的其他函数，以及以 term（变量、常量、算术、`NOW`，以及它们的 `STR`、`LANG`、`DATATYPE`、`ABS`、`ROUND`、`CEIL`、`FLOOR`、`IF`、`COALESCE`、字符串函数或 dateTime 函数）以外的表达式作为操作数，例如作为 `IF`、`COALESCE` 或 `IN` 参数的比较；
- 模式的匹配在 XPath 与 PostgreSQL 中可能不同、引用第九个之后的分组，或模式、替换字符串、标志不是常量的 `REPLACE`；
- 非常量或不是 `xsd:integer`（例如 `1.5`）的 `SUBSTR` 起始位置与长度：SPARQL 1.1 中为类型错误，而像 `fn:substring` 那样取整的引擎会给出答案；
- 作用于 `date` 或 `timestamp` 列的字符串函数，其无穷值为 plain literal；
- 作用于 `xsd:date` 与 `xsd:time` 值的 dateTime 函数，以及 `NOW` 的 `sameTerm`；
- 在编码不是 UTF-8 的数据库中进行字符串排序、`REGEX` 与字符串函数。

## BIND 与 SELECT 表达式

`BIND(expr AS ?v)` 与 `SELECT (expr AS ?v)` 在每个解中将表达式的值绑定到 `?v`（SPARQL 1.1 §18.5，Extend）。表达式为变量、常量、与 [`FILTER`](#filter) 中相同的算术与数值函数、`STRLEN`，或结果为算术或数值常量的 `IF` 与 `COALESCE`：

- 变量给出其 term，常量给出其自身。
- 算术给出一个数，其数据类型遵循 XPath 的类型提升，并以规范形式写出（见[字面量](#字面量)）。在整数列上，`?qty * 2` 为 `xsd:integer`，`?qty / 4` 为 `xsd:decimal`。XPath 将 decimal 除法的精度留给实现决定，此处采用 PostgreSQL `numeric` 除法的精度。`?qty * 1.5e0` 为 `xsd:double`，例如 `1.5E0`、`-0.0E0`、`INF` 或 `NaN`。`SELECT DISTINCT` 区分 `-0.0E0` 与 `0.0E0`，它们是不同的 term。
- `ABS`、`ROUND`、`CEIL` 与 `FLOOR` 给出与其参数数据类型相同的数。在 `numeric` 列上，`ROUND(?price)` 为 `xsd:decimal`，例如 `3`；在整数列上，`ROUND(?qty)` 即 `?qty`。
- `YEAR`、`MONTH`、`DAY`、`HOURS` 与 `MINUTES` 给出 `xsd:integer`，`SECONDS` 给出 `xsd:decimal`，例如 `58.25` 或 `0`。无穷 timestamp 使 `?v` 保持未绑定。
- `IF` 与 `COALESCE` 给出其所选参数的数值。在 `?n` 为零或未绑定的解中，`COALESCE(?qty / ?n, 0)` 为 `0`。在 `?n` 未绑定的解中，`IF(?n > 0, ?qty / ?n, 0)` 的条件为错误，因此 `?v` 保持未绑定。
- 发生错误时（例如除以零，或操作数不是数字），`?v` 保持未绑定。该解仍被保留。

SPARQL 在 `BIND` 所在组的解上对其求值。v12n 在连接之后，在整个模式的解上计算它。当表达式中该组可能未绑定的变量不被模式的其余部分绑定时，两者给出相同的值。

以下情形作为不支持的功能拒绝：

- 变量、常量、算术、数值函数、`STRLEN`、`YEAR` 至 `SECONDS`，以及对算术与数值常量的 `IF` 与 `COALESCE` 以外的表达式，例如 `STR`、比较、`IN`、`UCASE` 等字符串函数、`TZ`、`TIMEZONE`、`NOW`，以及结果可以是任意 term 的 `COALESCE(?x, 0)`。在 SQL 中，计算得到的变量只保存数值；
- `OPTIONAL` 或 `UNION` 内部的 `BIND`；
- 对其所在组可能未绑定、而模式后续部分会绑定的变量使用 `BIND`；
- 在三元组模式、`FILTER`、`ORDER BY` 或其他表达式中使用由 `BIND` 或 `SELECT` 计算的变量。

## VALUES

`VALUES` 在组内或查询之后提供内联数据（SPARQL 1.1 §10.2），并与模式连接（§18.5，Join）。当一行的每个 term 都是解绑定到其变量的 term 时，该行与该解兼容；`UNDEF` 与任何 term 兼容（§18.3）。

`VALUES` 中的常量与[三元组模式中的常量](#查询中的常量)一样作为 RDF term 匹配。每一行是 SQL 中析取的一个分支，行中的每个常量按三元组模式常量的方式与其变量的列比较。在 shop 示例中，`?o ex:placedBy ?c ; ex:amount ?a` 与 `VALUES (?c ?a) { (<…/customer/7> UNDEF) (<…/customer/8> 30000) }` 给出以下条件：

```sql
(t0."customer_id" = $1 OR (t0."customer_id" = $2 AND t1."amount" = $3))
```

PostgreSQL 将每个参数的类型推断为与其比较的列的类型，因此 round trip 检查适用，并会使用该列的索引。SQL `VALUES` 列表会在连接将参数与列比较之前把参数类型定为 `text`。

若某行的常量无法由 term map 产生（例如 customer 模板对应的 order IRI），该行不进入该分支。没有任何行能够匹配的分支不进入查询。无法匹配该列任何值的常量只使其所在行失败。

SPARQL 将数据与其所在组的解连接。v12n 将其与整个模式的解连接。当该块位于 `OPTIONAL` 与 `UNION` 之外，且数据的每个变量在组其余部分的每个解中都被绑定时，两者给出相同的解。此时每个解要么保留一次，要么被丢弃，这与没有两行能匹配同一个解时的连接结果相同。

一个查询的所有 `VALUES` 块合计最多包含 1000 个值，即行数乘以变量数，包括 `UNDEF`。超过时查询失败，错误中写明该限制（`v12n query` 以 1 退出，HTTP 端点返回 400）。

以下情形作为不支持的功能拒绝：

- `OPTIONAL` 或 `UNION` 内部的 `VALUES`；
- 对组其余部分可能未绑定的变量使用 `VALUES`，例如没有三元组模式绑定的变量，或只有 `OPTIONAL` 部分绑定的变量；
- 能匹配同一个解的两行：相同的行，或仅在其中一行为 `UNDEF` 的位置不同的行；
- 没有变量的 `VALUES`。

## MINUS

`A MINUS { B }` 保留 `A` 中未被 `B` 的任何解移除的解（SPARQL 1.1 §8.3，§18.5 Minus）。当 `B` 的一个解与 `A` 的一个解兼容，且二者至少共享一个双方都绑定的变量时，`B` 的该解移除 `A` 的该解。`B` 的变量不出现在解中。

`B` 的每种 triples map 选择，以及 `B` 内 `UNION` 的每一侧，在 SQL 中都是一个 anti-join：对其表的 `LEFT JOIN`，其 `ON` 子句在它移除左侧行时成立，并在 `WHERE` 中对其一列使用 `IS NULL`。在 shop 示例中，`?o ex:placedBy ?c ; ex:amount ?a` 与 `MINUS { ?c ex:tier "VIP" }` 给出以下 SQL：

```sql
FROM "orders" t0
JOIN "orders" t1 ON t1."id" = t0."id"
LEFT JOIN "customers" t2 ON t2."tier" = $1 AND t2."id" = t0."customer_id"
WHERE t2."id" IS NULL AND …
```

- 共享变量与连接中一样作为 RDF term 比较。字符串永远不等于由相同数字组成的整数，不同 SQL 类型的列也可以比较。
- 只由 `OPTIONAL` 部分绑定的共享变量可能在一侧未绑定。此时它与任何值兼容，与 [`OPTIONAL`](#optional) 相同。只有双方都绑定时才算共享，因此除非某个共享变量在每一行中都被绑定，`ON` 子句还要求某个共享变量在双方都被绑定（`IS NOT NULL`）。
- 与左侧没有共享变量、或永远不可能与左侧兼容的 `B` 的选择不移除任何解，也不添加 `LEFT JOIN`（§8.3.2）。
- `B` 独立求值（§8.3.3）。其中的 `FILTER` 只看到 `B` 的变量，左侧的变量在其中未绑定。
- `MINUS` 作用于组中写在它之前的部分。它之后的模式与剩余的行连接，组的 `FILTER` 作用于整个组。
- 唯一的表是左侧的一个表、并通过 `NOT NULL` 唯一键连接的 `LEFT JOIN`，与 `OPTIONAL` 一样改为读取该行。

以下情形作为不支持的功能拒绝：

- `OPTIONAL`、`UNION` 或另一个 `MINUS` 内部的 `MINUS`；
- `MINUS` 内部的 `BIND` 与 `VALUES`。

## EXISTS 与 NOT EXISTS

`FILTER EXISTS { P }` 保留这样的解：将解中的变量替换为其值之后，`P` 有解（SPARQL 1.1 §8.1，§18.6 substitute）。`FILTER NOT EXISTS { P }` 保留其余的解。二者可以通过 `&&`、`||` 与 `!` 与其他条件组合，且永远不会出错。`P` 的变量不出现在解中。

`P` 的每种 triples map 选择，以及 `P` 内 `UNION` 的每一侧，在 SQL 中都是一个相关 `EXISTS` 子查询，其中任一子查询有行时 `EXISTS` 成立。在 shop 示例中，`?c ex:name ?n` 与 `FILTER EXISTS { ?o ex:placedBy ?c ; ex:amount ?a }` 给出以下 SQL：

```sql
FROM "customers" t0
WHERE EXISTS (SELECT 1 FROM "orders" t1
JOIN "orders" t2 ON t2."id" = t1."id" WHERE t1."customer_id" = t0."id" AND …) AND …
```

- `P` 绑定的解变量与连接中一样，作为 RDF term 与其值比较。字符串永远不等于由相同数字组成的整数，不同 SQL 类型的列也可以比较。
- `P` 内的 `FILTER` 读取解变量的值，在 `P` 中不绑定该变量的组里也是如此。`MINUS` 的右侧看不到这些值（§8.3.3）。
- 与解没有共享变量的 `P` 独立检查。有解的模式的 `NOT EXISTS` 移除所有解，而 `MINUS` 不移除任何解（§8.3.2）。
- 在 `OPTIONAL` 的 `FILTER` 中，子查询位于 `ON` 子句中，并读取两侧的变量（§18.5 LeftJoin）。
- 支持 `P` 内的 `OPTIONAL` 与 `UNION`。
- term 永远不可能是解的 term 的 `P` 的选择不添加子查询。没有任何子查询的 `EXISTS` 为假。

以下情形中，替换与和解的连接给出不同的结果，或没有测试用例覆盖，因此作为不支持的功能拒绝：

- 对在被过滤的解中可能由 `OPTIONAL` 留下未绑定的变量使用 `EXISTS`；
- 对 `P` 只在 `OPTIONAL` 内部绑定的解变量使用 `EXISTS`；
- `P` 内部的 `MINUS`、`BIND`、`VALUES`、子查询与 `EXISTS`，以及没有三元组模式的 `P`；
- `MINUS` 内部与 `IF` 条件内部的 `EXISTS`；
- `BIND`、`SELECT` 表达式与 `ORDER BY` 中的 `EXISTS`。

## ORDER BY、LIMIT 与 OFFSET

解修饰符按代数定义的顺序应用（SPARQL 1.1 §18.2.5）。该顺序为 `ORDER BY`，然后是投影、`DISTINCT`、`OFFSET` 与 `LIMIT`。它们全部在 SQL 中应用。

`ORDER BY` 接受变量以及 `FILTER` 支持的表达式。每个比较项为 `ASC`（默认）或 `DESC`。解按 SPARQL 的顺序排列（§15.1），从先到后依次为：

1. 无值（未绑定变量，或为错误的表达式）
2. 空白节点
3. IRI，按码位
4. 字面量。当 `<` 运算符能对两个字面量排序时，与 `FILTER` 一样按 `<` 排序。数字按值并采用 XPath 类型提升，字符串按码位，`false` 在 `true` 之前，dateTime 按时刻。

在 SPARQL 未规定顺序之处，引擎采用自身的固定顺序。该顺序在 Discussion #63 中确定。

- 不同种类的字面量按以下顺序排列：数字、布尔值、dateTime、字符串、带语言标签的字面量，然后是其他数据类型。
- 带语言标签的字面量先按词法形式，再按语言标签排序。
- 其他数据类型的字面量按数据类型 IRI 排序。之后，对 `date` 与 `time` 列按值排序，其他情况按词法形式排序。
- NaN 排在所有数字之后。`numeric` 的 NaN 与无穷值位于 PostgreSQL 放置它们的位置。数据类型不同的相等数字（`1` 与 `1.0`）之间没有特定顺序。
- `date` 与 `timestamp` 列的无穷值是普通字面量（`"infinity"`），在字符串之间排序。

`DESC` 将上述全部顺序反转，因此未绑定值排在最后。

每个比较项变为若干个 SQL 排序键（term 的种类，然后是每个种类对应的值）。借助这些键，不同 SQL 类型的值以及不同 `UNION` 分支的值可以一起排序。

当单个普通 `SELECT` 读取某个比较项的列，且该列中不可能有其他内容时（非可选的数值、布尔或 dateTime 列），SQL 直接按该列排序。此时，该列上的索引可以按顺序提供行。字符串与 IRI 在 `COLLATE "C"` 下排序，其他排序规则的索引无法用于此排序。

排序键作为额外的列选取，从不属于结果。SQL 不会经由子查询保持顺序，因此由最外层查询决定行到达的顺序。因此，引擎包裹在另一查询外层的每个查询（行数统计、将列转换为文本）都会再次按排序键排序。

`v12n explain` 没有列类型。它显示的每一列都按字符串排序。

`LIMIT` 与 `OFFSET` 是 SQL 的绑定参数。查询的 `LIMIT` 不超过行数上限时，最多保留该数量的解，因此结果从不被报告为截断。查询的 `LIMIT` 超过行数上限时，行数上限仍然适用。

以下情形作为不支持的功能拒绝：

- 对 `SELECT DISTINCT` 未投影的变量使用 `ORDER BY`。一个投影后的解可能代表多个被 `ORDER BY` 排在不同位置的解。
- 作用于带时区与不带时区的值的比较项；
- 作用于 `real` 列的比较项；
- 作用于编码不是 UTF-8 的数据库中字符串的比较项；
- 与 `FILTER` 中一样，其他函数、算术、`ABS`、`ROUND`、`CEIL`、`FLOOR`、`IF`、`COALESCE`、`STRSTARTS`、`STRENDS` 与 `CONTAINS` 以外的字符串函数、dateTime 函数、`NOW`，以及 SQL 无法计算的字符串形式。
