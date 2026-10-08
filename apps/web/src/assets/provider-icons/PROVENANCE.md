# Provider and lab icons: provenance

Brand SVGs copied, unmodified, from LobeHub's Lobe Icons. They are bundled into the SPA at build time; nothing is fetched from a CDN or any other host at runtime.

| | |
| --- | --- |
| Repository | https://github.com/lobehub/lobe-icons |
| Commit | `c385b2b8d1f9e19aa86e628d4e23c91ee1111a47` (2026-10-07T01:04:22+08:00, "chore(deps): bump @lobehub/ui to ^5.56.0 (#429)") |
| Source path | `packages/static-svg/icons/` (npm package `@lobehub/icons-static-svg` 1.95.1) |
| Documentation | https://icons.lobehub.com/ |
| License | MIT, Copyright (c) 2023 LobeHub. The full text is in [`LICENSE`](./LICENSE), copied byte for byte from the repository root. |
| Fetched | 2026-10-08, by `git clone` of the default branch; hashes were also checked against `raw.githubusercontent.com/lobehub/lobe-icons/<commit>/…`. |

## Brand use

The repository's `LICENSE` is the standard MIT text. At the pinned commit the repository contains no separate trademark, brand-guideline or logo-usage notice (searched README, docs and package READMEs for "trademark", "brand guideline" and "disclaimer"). The MIT license covers LobeHub's SVG artwork; it grants no rights in the marks themselves, which belong to their respective owners. The dashboard uses them only nominatively, to identify the provider or model lab next to its text name, and never as an endorsement.

## Files

`<name>.svg` is the monochrome ("Mono") variant, which uses `fill="currentColor"`. `<name>-color.svg` is the colored variant, copied where upstream provides one. `provenance.json` records the same SHA-256 values; `provenance.test.ts` fails if a file is added, removed or edited without updating both, and checks that every SVG is free of scripts, event handlers, external references and `<foreignObject>`.

The component (`src/components/provider-icon.tsx`) does not inject the files as HTML. It parses each file's allowlisted elements (`svg`, `title`, `defs`, `linearGradient`, `stop`, `path`) into React elements, drops the root `style` attribute and `<title>` (icons are decorative), and prefixes gradient IDs per instance.

| File | SHA-256 |
| --- | --- |
| `anthropic.svg` | `e833fdfa7e7187a86a05b870925067556a506dcd4239089aed73e1e58c9366a3` |
| `aws-color.svg` | `b23f044479fe5c80f7df0a9515f969e5ea710c5f40d26dffa08969a0151731f3` |
| `aws.svg` | `f73f2506defea5df3b064bcea998339e183c4f979b2bb4af4fcbf42c29ae151c` |
| `bedrock-color.svg` | `a149a5678ce2b5d821759aabca7b815867027a8231a76723b3673580e8177a60` |
| `bedrock.svg` | `b07f07bb6b77a1ee83916430250dc9adfc799ec43fbc5932947af099bcfcb9f9` |
| `bfl.svg` | `c08f545c77c40f2447fb91d75f78db81dc2aeebb7291bb51ad2a982276219dc7` |
| `chatglm-color.svg` | `6e935f6ccad7a776edef259b33bbbfe5b51d6486318d4a0f423708ffe2a27287` |
| `chatglm.svg` | `9dbb651f279cbd5e4197c5ed27c30336411f5a3496261f35f6bf04b324b6676f` |
| `claude-color.svg` | `a3101f3047a119aa11825ad9369510f0c472428c8c52d420e31bc62db44a8364` |
| `claude.svg` | `365a70a7eb3956d9b9a96086058ebe04e1dbd8e291a756ad964e8a283fbd6d38` |
| `cloudflare-color.svg` | `cee35d3f0ecb7925ce0a89aeaff8b907cadae7c3fe44a23e4001cc8d5ee57502` |
| `cloudflare.svg` | `78f8973dad59f8af7c4042ef6be451d809a4e14bdb2151a75a76b4e0811fd22f` |
| `cohere-color.svg` | `84d0ee3cbe66f030e5a18cb2c86da9166ab2137c7a98781693bb1fbf31e392b9` |
| `cohere.svg` | `72851dd36d6ab017f535202744765eead8f99cfb5ced77e1840bfdb70db7a85c` |
| `deepseek-color.svg` | `deba5f98a5c1796e20fcac3149bcd7eb8a32f0bdd04d048819400b1f28bd1439` |
| `deepseek.svg` | `8f9443e351b6dacce71871790201b799d92e8bf96a45ef79aeb5e9bf4db423e2` |
| `flux.svg` | `c7d557466f39a895b1cd0d233a64c092ef61ba052160af7db20af37464a9138a` |
| `gemini-color.svg` | `8ab0a9bafec11f7e69bcb9fc4ffd8f1bc927d1ddcbbb6ff36dee5ae8b5a9d602` |
| `gemini.svg` | `87d5b3c4be75a66f54c1936482a263df68185545b741129badd1b7c2449c18d3` |
| `google-color.svg` | `4e6aa8892ef15a6b431f40f2c9045979bf7e2357524e7bbb89b353cf4f32a5ac` |
| `google.svg` | `c4bb45d362cb0a98b33b1f2fb424db2a5cd95a90e1043ae4e387de81a5e64951` |
| `grok.svg` | `9175fc90c22655160231976c849f25a03b888d7cc0e04c5f1b987b659bb07c95` |
| `huggingface-color.svg` | `5d39d66bb6c9b026d3cb5de7bd9978dad3906570df2aca8f00078a6f8a3d0f5e` |
| `huggingface.svg` | `de7f2c60f974b75b385116ef0dc6a9fa62f2fd7bf58156b5623301a5436b91c4` |
| `liquid.svg` | `4727b1cbd33bd256aca5be79a7d91a4f2753b2feb0dcfbb7b9fb3f23928c2467` |
| `meta-color.svg` | `adf9f2c1a646ccd3a37ca8c2e7e5985d64630cd633f4b95fba393d1d44e0578c` |
| `meta.svg` | `805ef9a35305393eb5a89be46b0708c9b119b3308ca44aaedd1457ff04857060` |
| `microsoft-color.svg` | `5cfb0ffa3231313c2968b5e3fedfcce8e2e96c61bde4572384c3889a77a80350` |
| `microsoft.svg` | `0617d6111e99845e7ab06dd4ff1e16c2c1ad381575b3204ef02dd71b716ffa4b` |
| `mistral-color.svg` | `722f74b289d95486b43662fe24fa883b333701296f618406cd0ed502299170b6` |
| `mistral.svg` | `a06cfa54e7deff7f7544175b006b7f8a03fbc5624c44f7d553a44d07ea96e629` |
| `nvidia-color.svg` | `8c941e4eb8b782eccaaea1240c059d20be33ab8eda28d7c9ed9b53ac802fe683` |
| `nvidia.svg` | `5a419b99e0ffdbfbe8caa7ec25581054eae03024da59cb860c54ea55ac8e7e73` |
| `ollama.svg` | `3a268218fb2e6e81fa31df70f70b51331625047794db81db21d35359428fae7a` |
| `openai.svg` | `a595df6b423920c67a7f8f73c063e4bfb72d415948097b6cac063a2366bb5186` |
| `openrouter-color.svg` | `17bede1b89166f824ee06753dc526a3f5e18b769706deaab09d04a3a98de1a78` |
| `openrouter.svg` | `ae671cc83de9bd45db97a47ed5e38a4e1bd9f93a12be495ba4b4f8b284026bca` |
| `perplexity-color.svg` | `8353f3ab20822f1a933224b0ea32cc39f0c32d5740f4af8c254b0f418e0a3a70` |
| `perplexity.svg` | `c66c64e9e3c273ef6c235f743808d67ffa7d482e8cbe4a79496a42b60333e1fe` |
| `qwen-color.svg` | `77f5768c66d08ce1d3d14e73373975c1bc0454be88c81523ddd0ffd7e2974029` |
| `qwen.svg` | `dcb3ba2f2b55ccbacbade0ca0bf98921fbaf8a07848972974b4a9bf8077376cf` |
| `vllm-color.svg` | `3f837bad8d7f85d19e860a089639289ddd7492e67dc1ac7bd0f129f8f19b8ae6` |
| `vllm.svg` | `5b025970b51c270775d0528a68ad96583372c94a2a94c4d92b40b74cbe9f7153` |
| `voyage-color.svg` | `96423656ee32fb76eba43b9030612c997889b056912ddbc9b249f960adb2e333` |
| `voyage.svg` | `0ee62d404c39764a5afeda633035cdd10a4d43d04f6d42671642c9a504d96ad6` |
| `xai.svg` | `89eb7de9f0d02a41cfecd9109e253d7fd3529e27467dee4254faa67f3ac21451` |
| `zai.svg` | `e748cb5108ce37b116d7a5ba97d37e0ae97eadf6849b0de11afb248e244a01e1` |
| `zhipu-color.svg` | `8174e65ff71647d5c705c920c953a82bd723d9e3e42887bdd4ffc9dfe51207a0` |
| `zhipu.svg` | `d4c3fe5ff92820b2ae5e3ddaaa30e6fc622cb746b75baad97f4bb2428fa25c5d` |
| `LICENSE` | `add9d7531d1b21646317a8958e38fc727506fa39d24bdecb44154d943c82753a` |

## Omitted

- **SGLang**: no SGLang icon exists in `packages/static-svg/icons` at the pinned commit. SGLang connections use the neutral fallback glyph.
- **Meta Llama**: there is no separate Llama icon; Llama models use the Meta icon.

## Refreshing

Clone the repository at a new commit, copy the same file names from `packages/static-svg/icons/`, then update the commit, this table and `provenance.json` together. Do not hand-edit SVGs here; the hashes must match upstream.
