---
name: planejar
description: Cria o plano de desenvolvimento de uma atividade do ferroDB (ex. um item do checklist do README como Heap File, B+Tree, Lexer). Use quando o usuário pedir para planejar, desenhar ou quebrar em etapas uma nova funcionalidade antes de implementar.
argument-hint: <atividade, ex. "Heap File">
---

# Planejar atividade

Atividade: **$ARGUMENTS**

Objetivo: produzir um plano concreto e revisável **antes** de escrever qualquer código. Nesta skill você não altera código de `src/` nem `tests/`.

## 1. Entender o contexto

1. Leia `README.md` (checklist de features) e `doc.md` (decisões de design já tomadas).
2. Leia os módulos de `src/` que a atividade toca ou dos quais depende (ex.: Heap File depende de `buffer_pool`, `slotted_page`, `rid`, `page`).
3. Identifique invariantes existentes que não podem quebrar. Exemplos atuais:
   - header comum de 12 bytes (LSN + checksum) em toda página;
   - `slot_id` de uma tupla nunca muda enquanto ela existir;
   - operações falham **sem efeito colateral** (checam antes de escrever);
   - formato em disco é little-endian;
   - offsets `u16` → página ≤ `MAX_PAGE_SIZE`.
4. Identifique em qual seção do `README.md` a atividade está: é ela que define a pasta do código (ver seção 2).
5. Se algo essencial estiver ambíguo (escopo, formato em disco, API pública), pergunte ao usuário antes de fechar o plano. Para o resto, escolha um padrão razoável e registre como decisão.

## 2. Organização do projeto

O código é dividido em uma pasta por seção do `README.md`, cada uma um módulo Rust (`src/<camada>/mod.rs`) com um arquivo por componente:

| Seção do README | Pasta | Exemplos de arquivos |
|---|---|---|
| Storage | `src/storage/` | `disk_manager.rs`, `page.rs`, `buffer_pool.rs`, `replacer.rs`, `slotted_page.rs`, `rid.rs`, `checksum.rs`, `heap_file.rs` |
| Data Types | `src/types/` | `value.rs`, `integer.rs`, `string.rs` |
| Tables | `src/table/` | `schema.rs`, `tuple.rs`, `table_heap.rs` |
| Indexes | `src/index/` | `b_plus_tree.rs` |
| SQL | `src/sql/` | `lexer.rs`, `parser.rs`, `ast.rs` |
| Query Planning | `src/planner/` | `logical_plan.rs`, `physical_plan.rs` |
| Query Execution | `src/execution/` | `seq_scan.rs`, `filter.rs`, `hash_join.rs` |
| Transactions | `src/transaction/` | `transaction_manager.rs`, `lock_manager.rs` |
| Recovery | `src/recovery/` | `wal.rs`, `checkpoint.rs` |
| Query Optimization | `src/optimizer/` | `cost_model.rs`, `statistics.rs` |
| Constraints | `src/constraint/` | `primary_key.rs`, `foreign_key.rs` |
| Connectivity | `src/connectivity/` | `cli.rs`, `protocol.rs`, `server.rs` |
| Security | `src/security/` | `auth.rs`, `role.rs` |
| Catalog | `src/catalog/` | `catalog.rs`, `column.rs` |

Regras:
- Coisas compartilhadas por todas as camadas (ex.: `config.rs`) ficam na raiz de `src/`.
- Cada `mod.rs` só declara os submódulos e reexporta o que é API da camada; lógica fica nos arquivos dos componentes.
- **Dependência só para baixo**: camadas de cima usam as de baixo (`execution` → `table` → `storage`), nunca o contrário. `storage` não importa de nenhuma outra camada.
- Testes de integração em `tests/<camada>_<componente>.rs` (ex.: `tests/storage_slotted_page.rs`).
- Seção nova no README → pasta nova, seguindo o mesmo padrão (nome em inglês, snake_case).
- Se algum arquivo que a atividade toca ainda estiver fora da sua pasta, a **primeira etapa** do plano é movê-lo (só mover e ajustar `use`, sem mudar comportamento, com `cargo test` verde).

## 3. Escrever o plano

Salve em `.claude/plans/<slug-da-atividade>.md` com as seções abaixo e mostre um resumo ao usuário.

```markdown
# Plano: <atividade>

## Objetivo
O que passa a existir quando terminar (1–3 frases). Qual item do README é marcado.

## Fora de escopo
O que fica explicitamente para depois.

## Conceito
Explicação curta de como a estrutura funciona em bancos reais (referência CMU 15-445 quando fizer sentido) e como vamos simplificar.

## Design
- Camada (seção do README) e caminho de cada arquivo novo/alterado (ex.: `src/storage/heap_file.rs`), com a responsabilidade de cada um
- Dependências entre camadas (confirmar que só apontam para baixo)
- Structs, enums e assinaturas públicas (em Rust)
- Enum de erro do módulo (`<Modulo>Error` com `Display` + `std::error::Error`)
- Layout em disco / em bytes (tabela de offsets), se houver
- Invariantes que o módulo garante

## Padrão SQL (ISO/IEC 9075)
Se a atividade tem comportamento visível ao usuário (tipos, SQL, NULL, erros): o que o padrão define, o que vamos seguir e os desvios conscientes. Onde o padrão é omisso, a referência é o PostgreSQL. Omitir a seção para camadas internas (storage, buffer pool).

## Decisões e trade-offs
Cada decisão com a alternativa descartada e o porquê.

## Etapas
Lista numerada de passos pequenos, cada um compilável e testável sozinho:
1. <passo> — arquivos: ... — testes: ...
2. ...

## Plano de testes
- Unitários (no `#[cfg(test)] mod tests` do módulo): casos felizes, limites (0, 1, máximo, máximo+1), erros
- Teste baseado em modelo / operações aleatórias, quando houver estado mutável
- Integração em `tests/<camada>_<componente>.rs`: persistência, reabrir arquivo, eviction, corrupção

## Riscos
O que pode dar errado e como vamos detectar.

## Documentação
O que atualizar em `doc.md` e `README.md`.
```

## Regras do plano

- Etapas pequenas: cada uma deve terminar com `cargo test` verde.
- Não proponha dependências novas no `Cargo.toml` sem justificar; o projeto implementa as coisas do zero por propósito didático.
- Toda API que pode falhar retorna `Result<_, <Modulo>Error>`; nada de `panic!`/`unwrap()` fora de testes.
- Prefira reaproveitar o que já existe (`BufferPoolManager`, `SlottedPage`, `Rid`, `checksum`) a criar paralelos.
- Termine perguntando se o usuário aprova o plano ou quer ajustes. A implementação é feita depois com `/executar`.
