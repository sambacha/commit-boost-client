# Signer SDK Validation Checklist (Stainless TS + Go)

This checklist validates the Stainless configuration and normalized OpenAPI spec for the Signer API.

## 1) Spec and config sanity

- [ ] `api/signer-api.yml` parses as valid OpenAPI 3.0.2.
- [ ] All `$ref` pointers resolve.
- [ ] Operation IDs exist and are unique:
  - `getPubkeys`
  - `requestSignature`
  - `generateProxyKey`
  - `getStatus`
- [ ] `stainless.yml` parses as valid YAML and references `api/signer-api.yml`.

## 2) Generated SDK shape

- [ ] SDK exposes:
  - `client.signer.getPubkeys()`
  - `client.signer.requestSignature(...)`
  - `client.signer.generateProxyKey(...)`
  - `client.management.status()`
- [ ] No pagination helpers are generated.
- [ ] Auth option is `apiKey` and defaults from `SIGNER_API_KEY`.

## 3) Type behavior

- [ ] `requestSignature` request type enforces required `type` and `object_root` plus one of `pubkey` or `proxy`.
- [ ] `SignatureResult` is a typed union (`BlsSignature | EcdsaSignature`), not opaque `any`.
- [ ] 401/404/500 error responses map to a typed `ErrorResponse` body.

## 4) Runtime behavior checks

- [ ] `signer.getPubkeys()` returns typed `GetPubkeysResponse`.
- [ ] `signer.requestSignature()` works for:
  - consensus + `pubkey`
  - proxy_bls + `proxy`
  - proxy_ecdsa + `proxy`
- [ ] `signer.generateProxyKey()` returns typed delegation payload and signature.
- [ ] `management.status()` returns plain string status from `text/plain`.

## 5) Developer-experience checks

- [ ] Method names are idiomatic and stable in TypeScript and Go.
- [ ] No generated method/resource naming depends on example fixture values.
- [ ] README snippets compile (or type-check) as written.

## TypeScript usage snippet

```ts
import { SignerClient } from "@commit-boost/signer-sdk";

const client = new SignerClient({
  apiKey: process.env.SIGNER_API_KEY,
});

const keys = await client.signer.getPubkeys();

const sig = await client.signer.requestSignature({
  type: "consensus",
  pubkey: keys.keys[0].consensus,
  object_root: "0x3e9f4a78b5c21d64f0b8e3d9a7f5c02b4d1e67a3c8f29b5d6e4a3b1c8f72e6d9",
});

const status = await client.management.status();
console.log(sig, status);
```

## Go usage snippet

```go
package main

import (
	"context"
	"fmt"
	"os"

	signer "github.com/commit-boost/signer-sdk-go"
)

func main() {
	ctx := context.Background()
	client := signer.NewClient(signer.WithAPIKey(os.Getenv("SIGNER_API_KEY")))

	keys, err := client.Signer.GetPubkeys(ctx)
	if err != nil {
		panic(err)
	}

	sig, err := client.Signer.RequestSignature(ctx, signer.RequestSignatureRequest{
		Type:       "consensus",
		Pubkey:     keys.Keys[0].Consensus,
		ObjectRoot: "0x3e9f4a78b5c21d64f0b8e3d9a7f5c02b4d1e67a3c8f29b5d6e4a3b1c8f72e6d9",
	})
	if err != nil {
		panic(err)
	}

	status, err := client.Management.Status(ctx)
	if err != nil {
		panic(err)
	}

	fmt.Println(sig, status)
}
```

## Deferred follow-up items

- Rust SDK design and generation pipeline.
- MCP server generation and tool filtering strategy.
- Optional `403` response modeling if authorization semantics require it.
