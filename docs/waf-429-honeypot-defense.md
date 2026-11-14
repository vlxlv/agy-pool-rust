# Google WAF 429 Honeypot Defense & Prompt Specifications

## 1. Background & Threat Model

### 1.1 The WAF Honeypot Mechanism
Starting around September, Google deployed a targeted WAF rule (e.g. Cloud Armor / GFE edge rule) aimed at multi-account proxy pools (CPA, One-API, New-API, etc.):
- **Trigger**: When an agentic request contains characteristic system prompt fingerprints (such as the standard `RFC 2119:` keywords clause commonly injected by Claude/Codex harnesses), upstream returns HTTP `429 Too Many Requests`.
- **The Anomaly**: Unlike standard quota exhaustion, this 429 response deliberately **omits `Retry-After`** and lacks standard Google RPC `RESOURCE_EXHAUSTED` / `quotaResetTimeStamp` details.
- **Why 429 instead of 403**:
  - A `403 Forbidden` response indicates an authorization/client violation; proxy pools typically halt the request immediately and refrain from retrying.
  - A `429 Too Many Requests` masquerades as temporary rate limiting. Standard proxy failover logic immediately puts the current account into cooldown and retries the exact same request using the next available backup account in the pool.

### 1.2 The Cascading Unmasking Trap (连坐捕鼠笼)
Because the violation is rooted in the **request payload itself** (a poison pill), switching accounts does not resolve the rejection. In a naive proxy pool:
1. Account 1 hits WAF 429 (no retry delay).
2. Proxy routes immediately to Account 2 -> hits WAF 429.
3. Proxy routes to Account 3, 4, 5...
4. Within 1 to 2 seconds, all dormant, clean backup accounts in the pool are forced to send the identical prohibited payload from the same client IP.
5. Google's WAF clusters the entire batch of tokens/accounts under a single pool identity, setting them up for coordinated suspension.

---

## 2. Client / Agent System Prompt (Zero-Fingerprint RFC 2119 Alternative)

When configuring developer tools, coding agents, or custom instructions (e.g., Cursor, Claude Code, Roo Code, Codex, Aider), **never include literal `RFC 2119` strings**. Use the following equivalent semantic directive prompt instead:

```markdown
# Execution Rigor & Directive Precedence

Adhere strictly to the following semantic directive hierarchy when interpreting instructions:

- Imperative Constraints ("Mandatory", "Enforced", "Non-negotiable"):
  Absolute operational boundaries. Zero deviation, compromise, or speculative refactoring permitted.
- Prescriptive Directives ("Recommended", "Standard Practice"):
  Standard implementation paths to follow unless a documented technical conflict exists.
- Discretionary Guidance ("Permitted", "Optional"):
  Advisory optimizations left to engineering judgment.

Strictly enforce explicit user specifications over default assumptions. Never perform unrequested structural refactoring or scope expansion.
```

---

## 3. Proxy Engineering Prompt (Claude / Codex Implementation Specification)

Use the following English prompt when instructing an LLM or developer to implement anti-honeypot defenses and request-scoped circuit breakers inside proxy pool projects:

````markdown
Please harden our proxy pool's upstream error handling and failover mechanism against upstream WAF honeypot traps (specifically fake 429 responses).

### Threat Model & Problem Statement
Upstream WAFs (such as Google Cloud Armor) employ a canary honeypot defense against multi-account proxy pools:
1. When a client payload contains specific agentic prompt fingerprints, the WAF returns an HTTP 429 (Too Many Requests) rather than 403, and deliberately omits standard rate-limiting metadata (e.g., no `Retry-After` header and no Google RPC `RESOURCE_EXHAUSTED` error details).
2. Naive proxy pools assume any 429 is a temporary account-level rate limit and immediately fail over to backup accounts.
3. Because the payload itself is toxic (request-level violation), retrying it causes all backup accounts in the pool to fail with 429 in rapid succession. This unmasks the entire pool's account topology and leads to mass account bans.

### Implementation Requirements

1. **Strict 429 Semantic Validation (Distinguish Real vs Fake 429)**
   - Do NOT treat arbitrary 429 responses as quota exhaustion.
   - Only trigger account failover/cooldown if the 429 response satisfies legitimate quota criteria:
     - Has a valid `Retry-After` header, OR
     - Contains standard Google RPC error metadata (e.g., `status: "RESOURCE_EXHAUSTED"` with `details` containing `quotaResetTimeStamp` or `retryDelay`).
   - If an upstream 429 lacks these valid quota markers (i.e. bare WAF block / HTML error / empty details):
     - Treat it as a non-retryable request-level rejection.
     - Immediately relay the 429 error directly back to the downstream client.
     - Do NOT put the account into cooldown and do NOT rotate/failover to other accounts in the pool.

2. **Request-Scoped Consecutive 429 Circuit Breaker**
   - Track upstream attempts within the scope of a single client request.
   - Even if 429 responses appear valid, limit account failovers for the same request to a maximum of 1 retry (maximum 2 account attempts total per request).
   - If a request triggers 429 across 2 consecutive accounts, immediately terminate the failover chain, abort further account rotation, and return the error to the client to protect the rest of the pool.

3. **Prompt Cloaking (Optional / Pre-flight Sanitization)**
   - Before dispatching generation requests upstream, scan payloads for known agent fingerprints (e.g., `"RFC 2119 keywords:"`, `"anthropic-billing-header"`, etc.).
   - Break up matched ASCII words using Zero-Width Space (`\u200b`) so that WAF plaintext regex matching fails while downstream LLM tokenizers continue to interpret the prompt correctly.

Please write clean, idiomatic code with focused unit tests verifying both real quota 429 failover and fake WAF 429 circuit breaking.
````

---

## 4. Architectural Summary

| Vector | Naive Proxy Pool | Hardened Defense (`agy-pool-rust` Pattern) |
| :--- | :--- | :--- |
| **Pre-flight Payload** | Forwards prompt unchanged | Pre-scans and injects zero-width spaces (`\u{200b}`) into sensitive prompt signatures (`src/proxy/cloak.rs`). |
| **Bare 429 (No Retry-After)** | Blindly rotates to next account | Validates response headers & RPC body. Rejects rotation if not a verifiable quota reset error. |
| **Repeated 429s on Same Req** | Loops through every account | Breaks after 1 failover. Halts cascading unmasking immediately. |
