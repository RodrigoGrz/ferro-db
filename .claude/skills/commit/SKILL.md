---
name: commit
description: Faz o commit de uma atividade do ferroDB. Só commita com fmt, clippy e testes verdes, inclui apenas os arquivos da atividade, valida o check no README e usa mensagem no padrão tipo/descricao-em-ingles. Não faz push sem aprovação. Use quando o usuário pedir para commitar ou salvar o trabalho no git.
argument-hint: [atividade — vazio = a atividade da conversa atual]
---

# Commit

Atividade: **$ARGUMENTS** (se vazio, use a atividade trabalhada nesta conversa ou o plano mais recente em `.claude/plans/`; se ainda assim não estiver claro, pergunte).

## 0. Ambiente

O repositório fica no WSL (Arch). Se o Claude estiver rodando no Windows (caminho `\\wsl.localhost\...`), o `git` do Windows recusa o repositório ("dubious ownership") e o `cargo` não existe lá. Rode **todos** os comandos pelo WSL com shell interativo (o `cargo` vem do asdf):

```powershell
wsl -d archlinux --cd /home/rodrigo/projects/personal/ferro-db -- bash -ic "<comando>"
```

Se o Claude já estiver rodando dentro do Linux, rode os comandos direto.

## 1. Separar os arquivos da atividade

1. Rode `git status --short` e `git diff` (e leia os arquivos novos não rastreados).
2. Classifique cada arquivo:
   - **da atividade**: código, testes, `doc.md` e `README.md` ligados a ela;
   - **fora da atividade**: qualquer outra mudança. Fica de fora do commit e é listada para o usuário no final.
3. Nunca inclua: planos de `.claude/plans/` (são locais, estão no `.gitignore`), `data.db` ou outros arquivos `.db` gerados, `target/`, arquivos temporários de teste, segredos.
4. Se um mesmo arquivo misturar mudanças da atividade com outras, **pare e pergunte** ao usuário o que fazer (`git add -p` é interativo e não funciona aqui).
5. Adicione ao stage **por caminho**: `git add <arquivo1> <arquivo2> ...`. Nunca use `git add -A`, `git add .` nem `git commit -a`.

## 2. Validar o README

1. Veja se a atividade corresponde a um item do checklist do `README.md`.
2. Se corresponder, o item tem que estar `- [x]` e o `README.md` tem que estar no stage.
3. Se ainda estiver `- [ ]`, **pare**, avise o usuário e pergunte se é para marcar agora (a atividade pode estar incompleta). Só siga depois da resposta.
4. Se a atividade não estiver no README (ex.: refactor, fix), siga sem mexer nele.

## 3. Gate: tudo verde

Os testes têm que rodar sobre **exatamente o que vai ser commitado**. Se houver mudanças fora da atividade, guarde-as temporariamente:

```sh
git stash push --keep-index --include-untracked -m "commit-skill: fora da atividade"
```

Rode, nesta ordem:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Depois, se criou o stash, restaure com `git stash pop` (mesmo que algum comando tenha falhado) e confirme com `git status` que nada se perdeu.

**Se qualquer comando falhar: não faça o commit.** Mostre ao usuário a saída relevante (testes que falharam, avisos do clippy, arquivos fora do `fmt`) e pare. Não corrija nada nem pule o gate por conta própria.

## 4. Mensagem do commit

Formato do título: `<tipo>/<o-que-foi-feito-em-ingles>`, em minúsculas e kebab-case, curto e descritivo.

| Tipo | Quando usar | Exemplo |
|---|---|---|
| `feat` | funcionalidade nova no banco | `feat/heap-file-insert-and-scan` |
| `fix` | correção de bug | `fix/slotted-page-compact-offset-overflow` |
| `add` | adiciona algo que não é funcionalidade: testes, documentação, arquivos de apoio | `add/buffer-pool-eviction-tests` |
| `chore` | manutenção sem mudar comportamento: refactor, mover arquivos, config, skills, dependências | `chore/move-storage-modules-to-folder` |

Se ajudar, adicione um corpo curto em inglês, depois de uma linha em branco, com os pontos principais em tópicos. Faça o commit com essa mensagem (use heredoc para mensagens de várias linhas) e não use `--no-verify`.

## 5. Push: só com aprovação

**Não faça push nesta etapa.** Mostre ao usuário um resumo para ele decidir:

```markdown
## Commit criado
`<hash curto>` <título>

## Arquivos no commit
<saída de git show --stat HEAD, resumida>

## Validação
fmt ✅ · clippy ✅ · testes ✅ (<N> passaram)
README: <item marcado | não se aplica>

## Push
Branch `<branch>` → `<remote>/<branch>`; commits que vão subir:
<git log --oneline <remote>/<branch>..HEAD>

## Ficaram de fora
<arquivos alterados que não entraram no commit, ou "nenhum">

Posso fazer o push?
```

Só rode `git push` depois de um "sim" explícito do usuário nesta conversa. Nunca use `--force`. Se o push for recusado (ex.: remoto à frente), mostre o erro e pergunte como seguir. Não faça rebase nem merge por conta própria.
