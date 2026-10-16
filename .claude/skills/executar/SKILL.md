---
name: executar
description: Implementa uma atividade do ferroDB seguindo o plano em .claude/plans/ em estilo TDD, com código Rust idiomático e limpo, seguindo o padrão SQL ISO/IEC 9075 sempre que possível e cargo fmt/clippy/test verdes. Use quando o usuário pedir para executar, implementar ou codar uma atividade já planejada.
argument-hint: <atividade ou caminho do plano>
disable-model-invocation: true
---

# Executar atividade

Atividade: **$ARGUMENTS**

## 0. Antes de começar

1. Localize o plano em `.claude/plans/` (pelo nome da atividade ou caminho passado). Se não existir, pare e sugira rodar `/planejar` primeiro.
2. Leia o plano inteiro e os módulos que ele cita.
3. Rode `cargo test` para confirmar que o ponto de partida está verde. Se não estiver, avise o usuário antes de mudar qualquer coisa.

## 1. Ciclo TDD por etapa

Para cada etapa do plano, em ordem, um comportamento por vez:

1. **Red** — escreva um teste para o próximo comportamento e rode `cargo test`. Ele **tem que falhar**, e pelo motivo esperado (asserção, não erro de compilação por acaso). Se passar de primeira, o teste não está provando nada: ajuste-o.
2. **Green** — escreva o mínimo de código para esse teste passar. Nada de implementar comportamento que ainda não tem teste.
3. **Refactor** — com tudo verde, limpe nomes, remova duplicação e simplifique. Rode os testes de novo.
4. Repita até a etapa estar completa e então valide:
   ```sh
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
   Só avance com os três limpos.
5. Diga ao usuário, em uma linha, qual etapa terminou.

Bug encontrado no caminho também segue TDD: primeiro um teste que reproduz o bug, depois a correção.

Se o plano se mostrar errado ou incompleto, **pare e explique** o desvio antes de seguir; depois atualize o arquivo do plano com a decisão.

## 2. Código limpo em Rust idiomático

**Nomes descritivos**
- O nome diz o que a coisa é ou faz: `free_space_end`, `tuple_offset`, `find_victim_frame`. Nada de `tmp`, `data2`, `x`, `handle`, `process`, `do_it`.
- Abreviação só quando é vocabulário do domínio (`rid`, `lsn`, `bpm`, `crc`).
- Booleanos lidos como pergunta: `is_dirty`, `has_free_slot`, `can_fit`.
- Funções curtas e com uma responsabilidade; extraia uma função com nome bom em vez de escrever um comentário explicando um bloco.

**Poucos comentários**
- O código deve se explicar pelos nomes. Comente só o *porquê* que não dá para ler no código (uma decisão de design, uma invariante, uma referência ao CMU/PostgreSQL).
- Nada de comentário que repete o código, nem `///` óbvio em cada função (`/// Returns the page id`).
- O `//!` do módulo é o lugar para documentar layout em bytes e invariantes; mantenha-o atualizado.

**Rust, não Java**
- Sem getter/setter para todo campo; campo `pub` quando não há invariante a proteger, método quando há.
- Não crie `trait` só para "ter uma interface" com uma única implementação; trait só quando há mais de uma implementação real ou é necessário para genéricos.
- Evite hierarquias e nomes cerimoniais (`AbstractX`, `XImpl`, `XFactory`, `XHelper`, `XUtils`, builder para struct de 2 campos).
- Use o que Rust oferece: `enum` com dados para variantes de estado, `match` exaustivo, newtypes (`struct PageId(u32)`) quando evitam confusão, funções livres no módulo quando não precisam de `self`, iteradores em vez de loops com índice, `Option`/`Result` com `?`.
- Um arquivo por componente coeso (a struct, seu enum de erro e funções auxiliares juntos), não "um arquivo por struct". Visibilidade mínima: `pub` só o que outro módulo ou teste de integração usa; `pub(crate)` quando basta.
- Ownership e borrowing no lugar de `clone()` para calar o compilador; `Rc<RefCell<_>>` só com justificativa.

**Onde colocar o código**

Uma pasta por seção do `ROADMAP.md`, cada uma um módulo (`src/<camada>/mod.rs`):

| Seção do roadmap | Pasta |
|---|---|
| Storage | `src/storage/` |
| Data Types | `src/types/` |
| Tables | `src/table/` |
| Indexes | `src/index/` |
| SQL | `src/sql/` |
| Query Planning | `src/planner/` |
| Query Execution | `src/execution/` |
| Transactions | `src/transaction/` |
| Recovery | `src/recovery/` |
| Query Optimization | `src/optimizer/` |
| Constraints | `src/constraint/` |
| Connectivity | `src/connectivity/` |
| Security | `src/security/` |
| Catalog | `src/catalog/` |

- Siga o caminho definido no plano. Compartilhado por todas as camadas (ex.: `config.rs`) fica na raiz de `src/`.
- `mod.rs` só declara submódulos e reexporta a API da camada; nada de lógica nele.
- **Dependência só para baixo** (`execution` → `table` → `storage`); `storage` não importa de outras camadas. Se precisar importar para cima, o design está errado: pare e discuta.
- Testes de integração em `tests/<camada>_<componente>.rs`.
- Mover arquivo para a pasta certa é uma etapa separada: só mover e ajustar `use`, com `cargo test` verde antes e depois, sem mudar comportamento na mesma etapa.

## 3. Regras de correção

- **Erros**: cada módulo tem seu enum `<Modulo>Error` com `impl Display` e `impl std::error::Error` (com `source()` quando embrulha outro erro) e `From` para conversão. Sem `thiserror`/`anyhow`.
- **Sem `unwrap()`, `expect()`, `panic!` ou indexação que possa estourar** em código de produção. Em testes, `unwrap()` é ok.
- **Falhar sem efeito colateral**: valide tamanho, espaço e limites antes de alterar qualquer byte ou estado.
- **Aritmética de bytes**: `u16::try_from`/`checked_*` em conversões e somas que podem estourar; nunca `as` silencioso em valores vindos do disco.
- **Formato em disco**: little-endian, offsets como constantes nomeadas (sem números mágicos).
- **Invariantes existentes** não podem quebrar (header comum LSN+checksum, `slot_id` estável, checksum verificado na leitura).
- **Dependências**: não adicionar crates ao `Cargo.toml` sem perguntar.
- **Unsafe**: proibido, salvo pedido explícito do usuário.

## 4. Padrão SQL (ISO/IEC 9075)

Sempre que possível, o comportamento visível ao usuário segue o padrão ISO/IEC 9075. É uma meta, não uma camisa de força: o projeto vai se adaptando, desde que o desvio seja consciente e documentado.

- **Tipos**: use os nomes e a semântica do padrão (`SMALLINT`, `INTEGER`, `BIGINT`, `NUMERIC(p, s)`/`DECIMAL`, `REAL`, `DOUBLE PRECISION`, `BOOLEAN`, `CHAR(n)`, `VARCHAR(n)`, `DATE`, `TIME`, `TIMESTAMP`). Overflow de inteiro e valor que não cabe no tipo são erro, nunca wrap silencioso.
- **NULL e lógica de três valores**: comparação com `NULL` resulta em `UNKNOWN`; `WHERE` só aceita `TRUE`; `IS NULL`/`IS DISTINCT FROM` para comparar nulos; agregações ignoram `NULL` (exceto `COUNT(*)`); `UNIQUE` permite vários `NULL`.
- **Sintaxe**: strings com aspas simples, identificadores com aspas duplas; identificador sem aspas não diferencia maiúsculas/minúsculas.
- **Erros**: associe cada erro visível ao usuário ao `SQLSTATE` do padrão (ex.: `22012` divisão por zero, `22003` valor numérico fora do intervalo, `23505` violação de unique, `42601` erro de sintaxe).
- **Onde o padrão é omisso ou "implementation-defined"** (ex.: posição de `NULL` no `ORDER BY` sem `NULLS FIRST/LAST`, precisão de `REAL`), use o PostgreSQL como referência.
- **Desvios**: quando não seguir o padrão (simplificação, feature ainda não suportada), registre no plano e na seção "Deviations from the SQL Standard" de `docs/data-model.md`, dizendo o que o padrão pede e o que o ferroDB faz.
- **Testes**: o comportamento do padrão vira teste, principalmente os casos com `NULL` e os erros com seu `SQLSTATE`.

Camadas internas (páginas, buffer pool, formato em disco) não são cobertas pelo padrão e seguem as regras da seção 3.

## 5. Regras de teste

- Nome dos testes no padrão `<coisa>_should_<comportamento>` (ex.: `insert_should_reuse_deleted_slot`).
- Cada teste verifica um comportamento, com a estrutura preparar → agir → verificar separada por linha em branco.
- Cobrir para cada função pública: caminho feliz, limites (vazio, 1, máximo, máximo+1) e cada variante de erro que ela pode retornar.
- Estruturas com estado mutável ganham um teste de operações aleatórias comparado com um modelo simples (`HashMap`, `Vec`), verificando invariantes a cada passo — como `random_operations_should_match_model`.
- Persistência/disco: teste de integração em `tests/<camada>_<componente>.rs` usando arquivo temporário removido no `Drop` (ver `TempDb`), cobrindo reabrir o arquivo e eviction.
- Nada de arquivos de teste deixados no repositório (ex.: `data.db`).

## 6. Fechamento

1. Rode a validação completa uma última vez (fmt, clippy, test).
2. Atualize a seção do componente em `docs/storage-engine.md` ou `docs/data-model.md` (em inglês, no estilo das existentes: layout, operações, decisões de design, testes, limitações). Mudou componente ou fluxo de dados → `docs/architecture.md`. Decisão de design importante → ADR em `docs/decisions/`.
3. Marque o item correspondente no `ROADMAP.md` (com links para o código e os testes) e atualize o status resumido do `README.md` se ele mudou.
4. Resuma para o usuário: o que foi feito, desvios do plano, pendências.
5. **Não faça commit** a menos que o usuário peça. Sugira rodar `/revisar` em seguida.
