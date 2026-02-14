<p align="center"><strong>SLRV Codex CLI</strong> is a research fork of the OpenAI Codex CLI with the <a href="https://slrv.md/">SLRV framework</a> injected at the agent level.</p>
<p align="center"><em>Research tool only. Not a production product and not an official OpenAI release.</em></p>
<p align="center">
  <img src=".github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>
</br>
If you want the <em>official</em> Codex in your code editor (VS Code, Cursor, Windsurf), <a href="https://developers.openai.com/codex/ide">install in your IDE.</a>
</br>If you are looking for the <em>cloud-based agent</em> from OpenAI, <strong>Codex Web</strong>, go to <a href="https://chatgpt.com/codex">chatgpt.com/codex</a>.</p>

---

## Quickstart

### Installing and running SLRV Codex CLI

Install from the GitHub Releases in this repository:

```shell
# 1) Download the release asset for your platform from:
#    https://github.com/olegische/slrv-codex/releases/latest
# 2) Extract the SLRV binary.
tar -xzf slrv-codex-<target>.tar.gz
chmod +x slrv-codex
```

Then run `slrv-codex` to get started.

<details>
<summary>Release asset names and platform mapping.</summary>

Each GitHub Release contains many executables, but in practice, you likely want one of these:

- macOS
  - Apple Silicon/arm64: `slrv-codex-aarch64-apple-darwin.tar.gz`
  - x86_64 (older Mac hardware): `slrv-codex-x86_64-apple-darwin.tar.gz`
- Linux
  - x86_64: `slrv-codex-x86_64-unknown-linux-musl.tar.gz`
  - arm64: `slrv-codex-aarch64-unknown-linux-musl.tar.gz`

Each archive contains a single entry with the platform baked into the name (e.g., `slrv-codex-x86_64-unknown-linux-musl`). Rename that extracted file to `slrv-codex` if you prefer a shorter command name.

</details>

### Using Codex with your ChatGPT plan

Run `codex` and select **Sign in with ChatGPT**. We recommend signing into your ChatGPT account to use Codex as part of your Plus, Pro, Team, Edu, or Enterprise plan. [Learn more about what's included in your ChatGPT plan](https://help.openai.com/en/articles/11369540-codex-in-chatgpt).

You can also use Codex with an API key, but this requires [additional setup](https://developers.openai.com/codex/auth#sign-in-with-an-api-key).

## Docs

- [**SLRV framework**](https://slrv.md/)
- [**Codex Documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).

## Trademark Notice

`Codex` and `OpenAI` are trademarks of OpenAI. This project is an independent research fork and is not affiliated with, endorsed by, or sponsored by OpenAI. Use of these names is nominative and for compatibility/reference purposes only.
