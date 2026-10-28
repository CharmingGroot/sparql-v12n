# Security policy

## Reporting a vulnerability

Report security problems privately through GitHub's
[private vulnerability reporting](https://github.com/CharmingGroot/sparql-v12n/security/advisories/new)
on this repository, not in a public issue, pull request or discussion.

Include what an attacker can do, the commit, and the steps to reproduce
it (a mapping, a query and, if needed, a schema). The maintainer aims to
answer within a week. Fixes go to `main` when ready, and the advisory
credits the reporter unless they ask otherwise.

## Supported versions

sparql-v12n is experimental and has no release yet. Only the latest
commit on `main` receives fixes.

## Scope

The engine runs SQL with the credentials it is given. Security problems
include:

- SQL built from query input: constants and other values from a SPARQL
  query must only reach PostgreSQL as bound parameters, and identifiers
  only from the mapping.
- A query that writes to the database or leaves the read-only session.
- A way around the resource limits (statement timeout, row limit, query
  size, concurrent requests) or a request that crashes the server.
- Database credentials, or result values, in logs or error messages.
- Results that include data the mapping does not expose.

Out of scope: a database user with more privileges than reading the
mapped tables (connect as a user that can only read), and TLS,
authentication and CORS of the HTTP endpoint, which belong to a reverse
proxy in front of it.
