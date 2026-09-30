---
name: revisar
description: Revisa código do ferroDB procurando bugs, pontos de melhoria e falta de cobertura de testes. Use quando o usuário pedir para revisar, auditar ou checar uma atividade, módulo ou as mudanças atuais.
argument-hint: [módulo, arquivo ou atividade — vazio = mudanças não commitadas]
---

# Revisar

Alvo: **$ARGUMENTS** (se vazio, revise as mudanças do `git diff` + arquivos novos não rastreados; se não houver mudanças, pergunte o alvo).

Esta skill **não altera código**. Ela gera um relatório; as correções só são aplicadas se o usuário pedir.

## 1. Coletar contexto

1. Leia os arquivos do alvo por inteiro e os módulos que eles usam.
2. Se houver plano em `.claude/plans/` para a atividade, leia-o: a implementação cumpre o plano?
3. Rode e registre o resultado:
   ```sh
   cargo build
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
4. Se `cargo llvm-cov` estiver instalado, rode `cargo llvm-cov --summary-only` (e `--text` no arquivo alvo) para ver linhas não cobertas. Se não estiver, faça a análise de cobertura manualmente (seção 4).

## 2. Bugs (prioridade máxima)

Procure ativamente, com cenário concreto de falha:

- **Aritmética**: overflow/underflow em `u16`/`u32`/`usize`, casts `as` truncando, `offset + len` passando do tamanho da página, off-by-one em limites.
- **Dados vindos do disco** tratados como confiáveis: header corrompido, `slot_count` absurdo, offsets fora da página → deve virar erro, nunca panic.
- **Efeito colateral em falha**: operação que retorna `Err` depois de já ter alterado bytes/estado.
- **Invariantes**: `slot_id` estável, checksum carimbado na escrita e verificado na leitura, página zerada tratada como nova, header comum preservado.
- **Buffer pool**: `pin_count` que não volta a zero, página suja descartada sem flush, frame reutilizado ainda pinado, `page_table` inconsistente com os frames, replacer desatualizado.
- **Endianness** e layout diferentes do documentado no `//!` do módulo / `doc.md`.
- `unwrap()`, `expect()`, `panic!`, indexação direta em código de produção.

## 3. Melhorias

- Duplicação de lógica que já existe em outro módulo.
- Números mágicos no lugar de constantes nomeadas.
- API pública maior que o necessário; nomes pouco claros.
- Erros genéricos demais (variante que não diz o que falhou).
- Custo desnecessário (cópias de `Vec`, alocações em loop, busca linear onde há índice).
- Documentação (`doc.md`, README, doc comments) desatualizada em relação ao código.

Só aponte melhoria que valha o custo; não liste preferências de estilo que o `cargo fmt`/clippy já resolvem.

## 4. Cobertura de testes

Para cada função pública do alvo, verifique se existe teste para:

- caminho feliz;
- limites (vazio, 1 elemento, exatamente cheio, cheio + 1);
- **cada variante** do enum de erro que ela pode retornar;
- persistência (sobrevive a flush + reabrir arquivo + eviction), quando mexe com disco;
- sequências aleatórias contra um modelo, quando há estado mutável;
- corrupção de disco detectada, quando lê bytes do disco.

Liste os buracos como testes concretos a escrever (nome no padrão `<coisa>_should_<comportamento>` + o que ele verifica).

## 5. Relatório

Formato da resposta:

```markdown
## Resultado
build / clippy / test: ✅ ou ❌ (com a saída relevante)

## Bugs
1. **[alta|média|baixa]** [arquivo.rs:linha](src/arquivo.rs#Llinha) — o defeito
   Cenário: entrada/estado → resultado errado. Correção sugerida: ...

## Melhorias
1. [arquivo.rs:linha](...) — sugestão e benefício

## Cobertura de testes faltando
1. `nome_should_comportamento` — o que cobre

## Aderência ao plano
O que ficou diferente do plano (se houver plano).
```

Ordene por severidade. Seja honesto: se não achou bugs, diga isso em vez de inventar. Termine perguntando se o usuário quer que você aplique as correções e escreva os testes faltantes.
