# RAG: контрольные вопросы (задача 22)

Модель `glm-5.3-flash`, индекс `rag/index.sqlite` (стратегия `structure`, 49 чанков, эмбеддинги `nomic-embed-text`), k = 4.
Вопросы и ожидания — `docs/control.json`.

Поиск: нужный раздел среди 4 найденных чанков — 9/10, на первом месте — 7/10.

## Сводка

| режим | покрытие ожиданий | вопросов полностью | ошибок | prompt tok (Σ) | completion tok (Σ) | время (Σ) |
|---|---|---|---|---|---|---|
| plain | 29/42 (69%) | 3/10 | 0 | 698 | 2775 | 90 с |
| rag | 42/42 (100%) | 10/10 | 0 | 17995 | 1557 | 43 с |
| mcp | 42/42 (100%) | 10/10 | 0 | 51375 | 2521 | 80 с |

## По вопросам

Покрытие — сколько групп ожидаемого (`must`) есть в ответе. «Контекст» — сколько их было в самих найденных чанках: если в контексте есть, а в ответе нет — промах генерации, если нет и в контексте — промах поиска.

| # | вопрос | нужный раздел | ранг в поиске | контекст | plain | rag | mcp |
|---|---|---|---|---|---|---|---|
| 1 | What are the three paradigms of retrieval-augmented generation described in the survey? | II. OVERVIEW OF RAG | — | 3/3 | 3/3 | 3/3 | 3/3 · 3 выз. · раздел не читал |
| 2 | What drawbacks of Naive RAG does the survey list? | Naive RAG | 1 | 4/4 | 3/4 | 4/4 | 4/4 · 2 выз. · раздел прочитан |
| 3 | Which techniques does the survey describe for query expansion and query transformation? | Query Optimization | 1 | 5/5 | 3/5 | 5/5 | 5/5 · 4 выз. · раздел прочитан |
| 4 | In index optimization, what is the Small2Big method and what metadata can be attached to chunks? | Indexing Optimization | 2 | 5/5 | 3/5 | 5/5 | 5/5 · 4 выз. · раздел прочитан |
| 5 | What is the 'Lost in the middle' problem and how does the survey suggest processing retrieved content because of it? | Context Curation | 3 | 4/4 | 2/4 | 4/4 | 4/4 · 2 выз. · раздел прочитан |
| 6 | Which adaptive retrieval methods does the survey name, and how do they decide when to retrieve? | Adaptive Retrieval | 1 | 4/4 | 4/4 | 4/4 | 4/4 · 3 выз. · раздел прочитан |
| 7 | Which three quality scores and four required abilities are used to evaluate RAG models? | Evaluation Aspects | 1 | 7/7 | 7/7 | 7/7 | 7/7 · 3 выз. · раздел прочитан |
| 8 | Which benchmarks and automated tools for evaluating RAG does the survey mention? | Evaluation Benchmarks and Tools | 1 | 5/5 | 3/5 | 5/5 | 5/5 · 2 выз. · раздел прочитан |
| 9 | According to the survey, how does adding irrelevant documents to the context affect the accuracy of RAG? | RAG Robustness | 1 | 2/2 | 0/2 | 2/2 | 2/2 · 2 выз. · раздел прочитан |
| 10 | How large a context can current LLMs handle according to the survey, and why is RAG still needed then? | RAG vs Long Context | 1 | 3/3 | 1/3 | 3/3 | 3/3 · 4 выз. · раздел прочитан |

## Ответы

### 1. What are the three paradigms of retrieval-augmented generation described in the survey?

**Ожидание:** Naive RAG, Advanced RAG and Modular RAG (an evolution: retrieve-read → pre/post-retrieval optimization → replaceable modules and flows).

**Источник:** II. OVERVIEW OF RAG · **проверяемые группы:** `naive`, `advanced`, `modular`

**Найдено RAG:**
1. Front matter, стр. 1 · cos 0.9911
2. I. INTRODUCTION (part 1/2), стр. 1 · cos 0.9903
3. V. AUGMENTATION PROCESS IN RAG > C. Adaptive Retrieval, стр. 11–12 · cos 0.9900
4. V. AUGMENTATION PROCESS IN RAG > A. Iterative Retrieval, стр. 10–11 · cos 0.9897

#### plain — 3/3

> The survey describes three evolutionary paradigms of RAG:
> 
> 1. **Naive RAG** — The earliest "Retrieve-Read" approach: a query is embedded, relevant documents are retrieved (e.g., via dense retrieval), and these are simply concatenated with the prompt for the LLM to generate an answer. It suffers from issues like retrieval quality problems, irrelevant or redundant chunks, and generation issues such as hallucination.
> 
> 2. **Advanced RAG** — An enhancement of Naive RAG that focuses on optimizing the pre-retrieval and post-retrieval stages. Pre-retrieval improvements include query rewriting, expansion, and routing; post-retrieval improvements include reranking and context compression to better feed retrieved content to the model.
> 
> 3. **Modular RAG** — The most flexible paradigm, which restructures the RAG framework into interchangeable modules (e.g., retrieval, reranking, memory, routing, fusion, task adaptation) and adaptable patterns. It supports iterative, recursive, and adaptive retrieval flows, going beyond the fixed "retrieve then read" pipeline and allowing customized designs for specific tasks.

#### rag — 3/3

> The survey describes three RAG paradigms: Naive RAG, Advanced RAG, and Modular RAG [1].

#### mcp — 3/3

_3 вызовов MCP, прочитано 3863 симв., разделы: —_

> The survey (rag-survey-2312.10997.pdf, Section II, pp. 2–4) describes three RAG paradigms:
> 
> 1. **Naive RAG** – the earliest "retrieve-read" approach: index documents, retrieve relevant chunks via similarity search, and generate an answer. It suffers from retrieval quality issues, weak indexing, and generation problems like hallucination.
> 2. **Advanced RAG** – refines Naive RAG by adding **pre-retrieval** optimization (query rewriting/expansion, etc.) and **post-retrieval** optimization (reranking, context compression) to improve retrieval quality.
> 3. **Modular RAG** – the most flexible design: the pipeline is broken into independent, reconfigurable modules (e.g., search, memory, routing, fine-tuning) that can be recombined, allowing iterative, recursive, and adaptive retrieval patterns.
> 
> Each paradigm represents a progressive enhancement over the previous one.

### 2. What drawbacks of Naive RAG does the survey list?

**Ожидание:** Retrieval challenges (low precision/recall, misaligned or irrelevant chunks), generation difficulties (hallucination, irrelevance, toxicity, bias), augmentation hurdles (disjointed/incoherent output, redundancy and repetitive answers).

**Источник:** Naive RAG · **проверяемые группы:** `precision|recall`, `hallucinat`, `redundan|repetit`, `incoheren|disjoint`

**Найдено RAG:**
1. II. OVERVIEW OF RAG > A. Naive RAG, стр. 2–3 · cos 0.9875
2. VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness, стр. 14 · cos 0.9875
3. II. OVERVIEW OF RAG, стр. 2 · cos 0.9865
4. II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · cos 0.9856

#### plain — 3/4, нет: `incoheren|disjoint`

> According to the survey (Gao et al., 2023), Naive RAG suffers from three main categories of drawbacks:
> 
> 1. **Retrieval quality issues**: 
>  - Low precision — retrieved chunks may contain irrelevant content, causing hallucinations and errors.
>  - Low recall — not all relevant knowledge needed by the query is retrieved.
>  - Outdated information — retrieval results may be stale.
> 
> 2. **Augmentation process issues**:
>  - Context noise — retrieved content with low relevance degrades outputs and can cause "middle distraction" (the model over-attends to middle-context information).
>  - Redundancy and repetition — similar passages appear repeatedly in the retrieved content.
>  - Conflicts between retrieved and internal knowledge — potentially leading to inconsistencies.
> 
> 3. **Generation quality issues**:
>  - Hallucination — the model may generate content not grounded in the retrieved context.
>  - Irrelevance — generated answers may not address the user's question.
>  - Toxicity or bias — harmful or biased content may appear in outputs.
> 
> In short, Naive RAG's "retrieval–read" pipeline can suffer from imprecise retrieval, noisy or redundant augmented context, and unreliable generation, which motivated the development of Advanced RAG and Modular RAG approaches.

#### rag — 4/4

> According to the survey, Naive RAG has three notable drawbacks [1]:
> 
> 1. **Retrieval Challenges** – The retrieval phase often struggles with precision and recall, leading to misaligned or irrelevant chunks being selected, and crucial information being missed.
> 
> 2. **Generation Difficulties** – The model may hallucinate, producing content not supported by the retrieved context; outputs can also suffer from irrelevance, toxicity, or bias.
> 
> 3. **Augmentation Hurdles** – Integrating retrieved information with different tasks can be challenging, producing disjointed or incoherent outputs. Redundancy from multiple similar sources can cause repetitive responses, and determining passage relevance and ensuring stylistic/tonal consistency add complexity. Additionally, a single retrieval based on the original query may not suffice for complex issues, and generation models may overly rely on augmented information, merely echoing retrieved content without adding insightful or synthesized output [1].
> 
> The development of Advanced RAG and Modular RAG was a response to these shortcomings [3].

#### mcp — 4/4

_2 вызовов MCP, прочитано 8918 симв., разделы: I. INTRODUCTION; II. OVERVIEW OF RAG; II. OVERVIEW OF RAG > A. Naive RAG; II. OVERVIEW OF RAG > B. Advanced RAG_

> The survey (Section II.A, "Naive RAG", rag-survey-2312.10997.pdf, pp. 2–3) lists three categories of drawbacks:
> 
> 1. **Retrieval Challenges** – The retrieval phase often struggles with precision and recall, selecting misaligned or irrelevant chunks and missing crucial information.
> 
> 2. **Generation Difficulties** – The model may hallucinate (produce content not supported by the retrieved context), and outputs can suffer from irrelevance, toxicity, or bias, reducing quality and reliability.
> 
> 3. **Augmentation Hurdles** – Integrating retrieved content with different tasks can yield disjointed or incoherent outputs; retrieving similar info from multiple sources causes redundancy and repetition; determining passage significance and ensuring stylistic/tonal consistency add complexity; a single retrieval based on the original query may not provide enough context for complex issues; and the model may overly rely on augmented information, merely echoing retrieved content without insightful synthesis.

### 3. Which techniques does the survey describe for query expansion and query transformation?

**Ожидание:** Expansion: Multi-Query, Sub-Query (least-to-most), Chain-of-Verification (CoVe). Transformation: query rewrite (RRR, BEQUE at Taobao), HyDE (hypothetical documents), Step-back prompting. Plus query routing.

**Источник:** Query Optimization · **проверяемые группы:** `multi-query|multi query|multiple quer`, `sub-quer|sub-question|sub quer|subquer`, `chain-of-verification|chain of verification|cove`, `hyde|hypothetical`, `step-back|step back`

**Найдено RAG:**
1. III. RETRIEVAL > C. Query Optimization (part 1/2), стр. 8–9 · cos 0.9898
2. II. OVERVIEW OF RAG > A. Naive RAG, стр. 2–3 · cos 0.9883
3. III. RETRIEVAL > E. Adapter, стр. 9–10 · cos 0.9881
4. I. INTRODUCTION (part 1/2), стр. 1 · cos 0.9880

#### plain — 3/5, нет: `chain-of-verification|chain of verification|cove`, `step-back|step back`

> In that survey (Gao et al., 2023), the techniques for query expansion and transformation described are:
> 
> **Query expansion:**
> - **Expansion with a generated (hypothetical) answer** — HyDE: instead of retrieving with the query directly, an LLM generates a hypothetical answer document, and retrieval is done using that document, which is semantically closer to the target.
> - **Multi-query expansion** — using an LLM to rewrite/reformulate the query into multiple different queries (e.g., via prompt engineering) and retrieving for each, merging results.
> 
> **Query transformation:**
> - **Query rewriting** — LLMs rewrite raw, colloquial, or ambiguous queries into clearer, more suitable ones; the rewrite can also be conditioned on retrieval results (e.g., a "rewrite–retrieve–read" loop), rather than using one-shot rewriting.
> 
> Additionally, for improving retrieval robustness it mentions:
> - **Query routing** — using language classifiers to route different queries to suitable RAG pipelines (e.g., different corpora, databases for different languages).
> - **Query decomposition** — breaking a complex query into simpler sub-questions answered separately (as in ITER-RetGen, where the model iteratively generates, retrieves, and refines answers across multiple iterations).
> 
> These methods aim to overcome the "imperfect retrieval" problem caused by vague, ambiguous, or poorly formulated queries.

#### rag — 5/5

> **Query Expansion** [1]:
> - **Multi-Query**: using prompt engineering to expand queries via LLMs, executed in parallel; expansions are deliberately designed, not random.
> - **Sub-Query**: decomposing a complex question into simpler sub-questions (sub-question planning), e.g., via least-to-most prompting, to contextualize and fully answer the original question.
> - **Chain-of-Verification (CoVe)**: expanded queries are validated by an LLM to reduce hallucinations, yielding more reliable queries.
> 
> **Query Transformation** (retrieving based on a transformed query instead of the original) [1]:
> - **Query Rewrite**: prompting LLMs (or smaller models like RRR — Rewrite-retrieve-read) to rewrite queries; BEQUE at Taobao improved recall for long-tail queries and raised GMV.
> - **HyDE**: generating hypothetical documents (assumed answers) and relying on answer-to-answer embedding similarity rather than query-based similarity.
> - **Step-back Prompting**: abstracting the original query into a high-level concept (step-back) question; both the step-back question and original query are used for retrieval and answer generation.

#### mcp — 5/5

_4 вызовов MCP, прочитано 12046 симв., разделы: III. RETRIEVAL > A. Retrieval Source; III. RETRIEVAL > B. Indexing Optimization; III. RETRIEVAL > C. Query Optimization; III. RETRIEVAL > D. Embedding; III. RETRIEVAL > E. Adapter_

> In the survey (Section III-C "Query Optimization", pp. 8–9 of rag-survey-2312.10997.pdf):
> 
> **Query Expansion** — expanding a single query into multiple queries to add context and nuance:
> - **Multi-Query**: prompt-engineered LLM expansion into several queries run in parallel.
> - **Sub-Query**: decomposing a complex question into simpler sub-questions (e.g., least-to-most prompting).
> - **Chain-of-Verification (CoVe)**: LLM validates the expanded queries to reduce hallucinations.
> 
> **Query Transformation** — retrieving based on a transformed query instead of the original:
> - **Query Rewrite**: LLM rewrites suboptimal queries; also dedicated models like RRR (Rewrite-Retrieve-Read) and BEQUE (Taobao, improved long-tail query recall).
> - **HyDE**: generating hypothetical documents (assumed answers) so similarity is computed answer-to-answer.
> - **Step-back Prompting**: abstracting the query into a high-level "step-back question", retrieving with both and using both results for generation.
> 
> (These are separate from the related **Query Routing** technique — metadata/semantic routers — described in the same section.)

### 4. In index optimization, what is the Small2Big method and what metadata can be attached to chunks?

**Ожидание:** Small2Big: sentences (small) are the retrieval unit, the preceding and following sentences are given to the LLM as (big) context. Metadata: page number, file name, author, category, timestamp — used to filter retrieval.

**Источник:** Indexing Optimization · **проверяемые группы:** `sentence`, `page`, `file name|filename|file`, `author`, `timestamp|time`

**Найдено RAG:**
1. III. RETRIEVAL > A. Retrieval Source (part 2/3), стр. 6–7 · cos 0.9836
2. III. RETRIEVAL > B. Indexing Optimization, стр. 8 · cos 0.9829
3. III. RETRIEVAL > D. Embedding, стр. 9 · cos 0.9818
4. III. RETRIEVAL > E. Adapter, стр. 9–10 · cos 0.9817

#### plain — 3/5, нет: `sentence`, `author`

> **Small2Big (Small-to-Big retrieval):** This is an index organization strategy where the retrieval unit and the usage unit are decoupled. The index is built on small chunks (to make retrieval more precise and semantically accurate), but when a small chunk is matched, its *parent* (larger, surrounding) chunk — i.e., the bigger chunk containing or preceding it — is actually fed to the LLM. This preserves fine-grained retrieval accuracy while giving the model enough broader context to generate good answers. It is related to the hierarchical index idea of linking chunks across multiple levels of granularity.
> 
> **Metadata attached to chunks:** Chunks can be enriched with metadata such as:
> - Page number, file name, and chapter/section information (document-level provenance),
> - References, bibliographic citations, timestamps or dates,
> - Key concepts, keywords, or brief summaries,
> 
> This metadata helps with retrieval filtering (e.g., restricting to date ranges or sections) and disambiguation — for instance, when a chunk's content is ambiguous or pronouns lack antecedents, metadata supplies context. A key challenge noted in the survey is that metadata can become noisy or redundant (e.g., excessive keywords or repeated page numbers), so pipelines typically include steps to clean and control metadata quality during chunk construction.

#### rag — 5/5

> Small2Big is a chunking approach where sentences (the "small" units) serve as the retrieval unit, while the preceding and following sentences are provided as "big" context to LLMs [2]. Metadata that can be attached to chunks includes page number, file name, author, category, and timestamp [2]. This metadata allows retrieval filtering to limit the search scope; for example, weighting document timestamps enables time-aware RAG, ensuring knowledge freshness [2]. Metadata can also be artificially constructed, e.g., paragraph summaries or hypothetical questions (Reverse HyDE) [2].

#### mcp — 5/5

_4 вызовов MCP, прочитано 6716 симв., разделы: III. RETRIEVAL > A. Retrieval Source; III. RETRIEVAL > B. Indexing Optimization; III. RETRIEVAL > C. Query Optimization_

> **Small2Big** (indexing optimization, chunking strategy, p. 8): since fixed-size chunks struggle to balance semantic completeness with context length, Small2Big uses sentences ("small") as the retrieval unit, while the preceding and following sentences ("big") are supplied as context to the LLM.
> 
> **Metadata attachments** (p. 8): chunks can be enriched with metadata such as **page number, file name, author, category, and timestamp**. Retrieval can then be filtered by this metadata to limit scope, and timestamps can be weighted for time-aware RAG to keep knowledge fresh. Metadata can also be artificially constructed — e.g., paragraph summaries or hypothetical questions (Reverse HyDE).
> 
> Source: rag-survey-2312.10997.pdf, p. 8 (Sec. II-B, "Chunking Strategy" and "Metadata Attachments").

### 5. What is the 'Lost in the middle' problem and how does the survey suggest processing retrieved content because of it?

**Ожидание:** LLMs focus on the beginning and the end of a long context and forget the middle; so retrieved content is post-processed: reranking (put the most relevant first) and context selection/compression (e.g. LLMLingua).

**Источник:** Context Curation · **проверяемые группы:** `beginning|start`, `middle`, `rerank|re-rank`, `compress|llmlingua`

**Найдено RAG:**
1. II. OVERVIEW OF RAG > A. Naive RAG, стр. 2–3 · cos 0.9827
2. III. RETRIEVAL, стр. 5 · cos 0.9818
3. IV. GENERATION > A. Context Curation (part 1/2), стр. 10 · cos 0.9818
4. VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness, стр. 14 · cos 0.9817

#### plain — 2/4, нет: `rerank|re-rank`, `compress|llmlingua`

> In the survey, "Lost in the middle" refers to the finding that large language models tend to attend most to content at the beginning and end of their input context, while information placed in the middle of a long prompt is often ignored or poorly used—even when it is relevant to the query.
> 
> Because of this, the survey suggests that when processing retrieved content, models should pay more attention to the middle portions of the retrieved passages rather than only the beginning and end, so that relevant evidence located mid-document is not overlooked. In practice, this motivates strategies such as reordering, truncating, or selectively highlighting the most relevant parts of retrieved documents when feeding them into the model, ensuring key information isn't buried in the middle of a long context where it would be lost.

#### rag — 4/4

> "Lost in the middle" is the phenomenon where, like humans, LLMs tend to focus only on the beginning and end of long texts while forgetting the middle portion; overly long contexts can trigger this problem [3].
> 
> Because of this, the survey says retrieved content typically needs further processing (Context Curation), including:
> - Reranking: reordering chunks to put the most pertinent results first, acting as both enhancer and filter, using rule-based methods (Diversity, Relevance, MRR) or model-based ones (BERT-series models, Cohere rerank, bge-raranker-large, GPT) [3].
> - Context Selection/Compression: avoiding the misconception that more retrieved documents are better, since excessive context adds noise; methods include LLMLingua (using small models like GPT-2 Small or LLaMA-7B to remove unimportant tokens), PRCA (trained information extractor), and RECOMP (information condenser trained with contrastive learning) [3].

#### mcp — 4/4

_2 вызовов MCP, прочитано 6397 симв., разделы: III. RETRIEVAL > E. Adapter; IV. GENERATION; IV. GENERATION > B. LLM Fine-tuning; IV. GENERATION > A. Context Curation; V. AUGMENTATION PROCESS IN RAG; V. AUGMENTATION PROCESS IN RAG > A. Iterative Retrieval_

> **"Lost in the middle"** (survey p. 10, citing [98]): like humans, LLMs reading long texts tend to focus only on the beginning and end, forgetting the middle portion. Overly long or redundant retrieved contexts therefore degrade generation.
> 
> Because of this, the survey (Section IV-A, "Context Curation") suggests further processing of retrieved content rather than feeding it all to the LLM:
> 
> 1. **Reranking** — reorder document chunks so the most pertinent ones come first and shrink the document pool; via rule-based metrics (Diversity, Relevance, MRR), model-based rankers (BERT-family, Cohere rerank, bge-reranker-large), or LLMs.
> 2. **Context selection/compression** — more docs ≠ better; excessive context adds noise. Tools: LongLLMLingua (SLM-based token compression), PRCA (trained information extractor), RECOMP (contrastive-learning condenser), and the "Filter-Reranker" paradigm (SLMs filter, LLMs reorder/critique) to reduce document count and noise.

### 6. Which adaptive retrieval methods does the survey name, and how do they decide when to retrieve?

**Ожидание:** FLARE triggers retrieval when the probability (confidence) of generated terms falls below a threshold; Self-RAG uses reflection tokens (retrieve / critic) so the model decides itself when to retrieve.

**Источник:** Adaptive Retrieval · **проверяемые группы:** `flare`, `self-rag|selfrag|self rag`, `reflection`, `probabilit|confiden|threshold`

**Найдено RAG:**
1. V. AUGMENTATION PROCESS IN RAG > C. Adaptive Retrieval, стр. 11–12 · cos 0.9903
2. V. AUGMENTATION PROCESS IN RAG > A. Iterative Retrieval, стр. 10–11 · cos 0.9877
3. III. RETRIEVAL > E. Adapter, стр. 9–10 · cos 0.9877
4. IV. GENERATION > A. Context Curation (part 2/2), стр. 10 · cos 0.9874

#### plain — 4/4

> In that survey (Gao et al., 2023), adaptive retrieval is discussed in the section on when to retrieve, and it names roughly three approaches:
> 
> 1. **Self-RAG (Asai et al., 2023)** — The LLM itself is trained to emit special "reflection tokens," including one that decides whether retrieval is needed at all for a given query (and even at generation steps). The model learns this judgment through instruction tuning, so retrieval is triggered only when the model predicts it would help.
> 
> 2. **FLARE (Jiang et al., 2023)** — Retrieves *during* generation rather than upfront: the model generates a tentative sentence, and if the confidence of the tokens being generated falls below a threshold, it treats the low-confidence sentence as a query to retrieve more information and then regenerates.
> 
> 3. **Adaptive-RAG (Jeong et al., 2024)** — A classifier judges the complexity of an incoming query and routes accordingly: simple queries are answered with no retrieval or a single retrieval step, while complex multi-hop queries trigger multi-step/iterative retrieval.
> 
> The common idea across these methods is to decide retrieval dynamically based on signals such as the model's own predicted need, generation confidence, or query complexity, instead of always retrieving or never retrieving — which improves efficiency and avoids distracting the model when retrieval is unnecessary.

#### rag — 4/4

> The survey names Flare [24] and Self-RAG [25] as exemplars of adaptive retrieval [1].
> 
> - **Flare** automates retrieval timing by monitoring the confidence of the generation process, measured via the probability of generated terms. When this probability falls below a threshold, retrieval is activated to gather relevant information [1].
> - **Self-RAG** introduces "reflection tokens" ("retrieve" and "critic") that let the model introspect its outputs. It autonomously decides when to retrieve (or a predefined threshold triggers it), then performs fragment-level beam search across paragraphs, using critic scores to update subdivision scores, with weights adjustable during inference. This design avoids extra classifiers or NLI models [1].
> 
> The survey also situates these within a broader trend of LLMs using active judgment, citing agents like AutoGPT, Toolformer, Graph-Toolformer, and WebGPT [1].

#### mcp — 4/4

_3 вызовов MCP, прочитано 11019 симв., разделы: V. AUGMENTATION PROCESS IN RAG > A. Iterative Retrieval; V. AUGMENTATION PROCESS IN RAG > B. Recursive Retrieval; V. AUGMENTATION PROCESS IN RAG > C. Adaptive Retrieval; VI. TASK AND EVALUATION; VI. TASK AND EVALUATION > A. Downstream Task; VI. TASK AND EVALUATION > B. Evaluation Target; VI. TASK AND EVALUATION > C. Evaluation Aspects_

> In the survey (Section V-C "Adaptive Retrieval," pages 11–12), the exemplar methods are **FLARE** and **Self-RAG**:
> 
> - **FLARE**: decides when to retrieve by monitoring the confidence of the generation process — if the probability of generated terms falls below a threshold, retrieval is triggered to collect relevant information.
> - **Self-RAG**: uses LLM-generated "reflection tokens" ("retrieve" and "critic") so the model introspects and autonomously decides when to activate retrieval (or a predefined threshold can trigger it); critic scores guide fragment-level beam search over retrieved paragraphs, with tunable weights at inference — no extra classifiers or NLI models needed.
> 
> The survey notes adaptive retrieval more broadly lets the RAG system autonomously determine whether retrieval is necessary and when to stop, often via special control tokens, in the spirit of agent-like models such as Graph-Toolformer and WebGPT (pp. 4–5, 11).

### 7. Which three quality scores and four required abilities are used to evaluate RAG models?

**Ожидание:** Quality scores: context relevance, answer faithfulness, answer relevance. Abilities: noise robustness, negative rejection, information integration, counterfactual robustness.

**Источник:** Evaluation Aspects · **проверяемые группы:** `context relevance`, `faithful`, `answer relevance`, `noise robust`, `negative rejection`, `information integration`, `counterfactual`

**Найдено RAG:**
1. VI. TASK AND EVALUATION > C. Evaluation Aspects (part 1/2), стр. 12 · cos 0.9936
2. VI. TASK AND EVALUATION > D. Evaluation Benchmarks and Tools, стр. 14 · cos 0.9925
3. II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · cos 0.9918
4. VII. DISCUSSION AND FUTURE PROSPECTS > C. Hybrid Approaches, стр. 14–15 · cos 0.9918

#### plain — 7/7

> In that survey (Gao et al.), RAG models are evaluated on:
> 
> **Three quality scores (for RAG, mainly in the generation/retrieval pipeline):**
> 1. **Context relevance** – the retrieved context should be relevant to the query (measures retrieval quality, precision/recall).
> 2. **Answer faithfulness** – the answer must stay faithful to the retrieved context (should not hallucinate beyond it).
> 3. **Answer relevance** – the final answer should be relevant to the original question.
> 
> **Four required abilities:**
> 1. **Noise robustness** – performing well even when retrieved documents contain noisy/irrelevant information.
> 2. **Negative rejection** – refusing to answer when the retrieved documents don't contain the needed knowledge.
> 3. **Information integration** – integrating knowledge from multiple retrieved documents to answer complex questions.
> 4. **Counterfactual robustness** – handling incorrect or conflicting information in retrieved documents (e.g., detecting and ignoring counterfactuals and relying on the model's own knowledge when appropriate).

#### rag — 7/7

> The three quality scores are [1]:
> 1. Context Relevance – precision and specificity of the retrieved context
> 2. Answer Faithfulness – answers remain true to the retrieved context
> 3. Answer Relevance – answers directly address the question
> 
> The four required abilities are [1]:
> 1. Noise Robustness – handling question-related but uninformative noise documents
> 2. Negative Rejection – refraining from answering when retrieved documents lack the needed knowledge
> 3. Information Integration – synthesizing information from multiple documents
> 4. Counterfactual Robustness – recognizing and disregarding known inaccuracies in documents
> 
> Context relevance and noise robustness evaluate retrieval quality; the rest evaluate generation quality [1].

#### mcp — 7/7

_3 вызовов MCP, прочитано 6745 симв., разделы: V. AUGMENTATION PROCESS IN RAG > C. Adaptive Retrieval; VI. TASK AND EVALUATION; VI. TASK AND EVALUATION > A. Downstream Task; VI. TASK AND EVALUATION > B. Evaluation Target; VI. TASK AND EVALUATION > C. Evaluation Aspects_

> Per Section V-C ("Evaluation Aspects", p. 12) of *Retrieval-Augmented Generation for Large Language Models: A Survey*:
> 
> **Three quality scores:**
> 1. **Context Relevance** – precision/specificity of the retrieved context.
> 2. **Answer Faithfulness** – answers must stay true to the retrieved context.
> 3. **Answer Relevance** – answers must directly address the question.
> 
> **Four required abilities:**
> 1. **Noise Robustness** – handling question-related but uninformative documents.
> 2. **Negative Rejection** – abstaining when retrieved docs lack the needed knowledge.
> 3. **Information Integration** – synthesizing information from multiple documents.
> 4. **Counterfactual Robustness** – recognizing and ignoring known inaccuracies in documents.
> 
> These collectively evaluate the two RAG targets: retrieval and generation.
> 
> (Source: rag-survey-2312.10997.pdf, p. 12)

### 8. Which benchmarks and automated tools for evaluating RAG does the survey mention?

**Ожидание:** Benchmarks RGB, RECALL, CRUD (essential abilities); automated tools RAGAS, ARES, TruLens that use LLMs to judge the quality scores.

**Источник:** Evaluation Benchmarks and Tools · **проверяемые группы:** `rgb`, `crud`, `ragas`, `ares`, `trulens`

**Найдено RAG:**
1. VI. TASK AND EVALUATION > D. Evaluation Benchmarks and Tools, стр. 14 · cos 0.9936
2. VI. TASK AND EVALUATION > B. Evaluation Target, стр. 12 · cos 0.9916
3. VII. DISCUSSION AND FUTURE PROSPECTS > C. Hybrid Approaches, стр. 14–15 · cos 0.9909
4. VIII. CONCLUSION, стр. 16–17 · cos 0.9904

#### plain — 3/5, нет: `rgb`, `crud`

> In "Retrieval-Augmented Generation for Large Language Models: A Survey" (Gao et al., 2023), the evaluation section mentions:
> 
> **Benchmarks:**
> - **KILT** – a benchmark integrating various knowledge-intensive tasks
> - **BEIR** – for evaluating zero-shot retrieval capabilities
> - Additional task-specific datasets, such as Natural Questions (NQ), TriviaQA, and HotpotQA, used in downstream evaluations of QA and open-domain tasks
> 
> **Automated evaluation tools/frameworks:**
> - **RAGAS** – a framework for reference-free evaluation of RAG pipelines, assessing aspects like faithfulness, answer relevance, and context relevance
> - **ARES** – an automated framework for estimating RAG performance across relevance, faithfulness, and other dimensions
> - **TruLens / TruEra-style evaluation** – discussed in the context of assessing RAG quality via feedback functions
> 
> The survey also highlights evaluation criteria/dimensions used by such tools: **context relevance** (retrieval quality), **answer faithfulness/groundedness** (generation faithful to retrieved content), and **answer relevance** (response to the query). These correspond to evaluating the two core RAG components—retrieval and generation—separately.
> 
> Note: the survey's discussion is relatively brief, and tool coverage may vary by version of the paper; RAGAS and ARES are the most prominently cited automated evaluators.

#### rag — 5/5

> The survey mentions the following benchmarks and automated tools for evaluating RAG:
> 
> **Benchmarks:** RGB, RECALL, and CRUD — these focus on appraising the essential abilities of RAG models [1].
> 
> **Automated tools:** RAGAS, ARES, and TruLens — state-of-the-art tools that employ LLMs to adjudicate quality scores [1].
> 
> Additionally, RALLE is mentioned as a tool for the automatic evaluation of RAG applications, based on task-specific metrics [2]. The survey also notes earlier task-specific evaluation metrics such as EM, F1, Accuracy, BLEU, and ROUGE [2], and summarizes the evaluation aspects of these frameworks in Table IV, covering retrieval quality (e.g., context relevance) and generation quality (e.g., faithfulness, answer relevance, noise robustness, counterfactual robustness) [3].

#### mcp — 5/5

_2 вызовов MCP, прочитано 8592 симв., разделы: VI. TASK AND EVALUATION > C. Evaluation Aspects; VI. TASK AND EVALUATION > D. Evaluation Benchmarks and Tools; VII. DISCUSSION AND FUTURE PROSPECTS; VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context; VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness; VII. DISCUSSION AND FUTURE PROSPECTS > C. Hybrid Approaches_

> In Section VI-D ("Evaluation Benchmarks and Tools", p. 14), the survey mentions:
> 
> **Benchmarks** (appraising the essential abilities of RAG models):
> - **RGB**
> - **RECALL**
> - **CRUD**
> 
> **Automated tools** (using LLMs to judge quality scores):
> - **RAGAS**
> - **ARES**
> - **TruLens**
> 
> The survey notes that together these form a framework for systematic, quantitative evaluation of RAG models across the evaluation aspects described earlier in Section VI (summarized in its Table IV). (File: rag-survey-2312.10997.pdf, p. 14.)

### 9. According to the survey, how does adding irrelevant documents to the context affect the accuracy of RAG?

**Ожидание:** Counter-intuitively, including irrelevant documents can increase accuracy by over 30% (contrary to the assumption that quality drops).

**Источник:** RAG Robustness · **проверяемые группы:** `30`, `increase|improve|higher|boost|rais`

**Найдено RAG:**
1. VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness, стр. 14 · cos 0.9921
2. III. RETRIEVAL, стр. 5 · cos 0.9890
3. VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context, стр. 14 · cos 0.9885
4. VI. TASK AND EVALUATION > C. Evaluation Aspects (part 1/2), стр. 12 · cos 0.9882

#### plain — 0/2, нет: `30`, `increase|improve|higher|boost|rais`

> According to the survey "Retrieval-Augmented Generation for Large Language Models: A Survey" (Gao et al., 2023), adding irrelevant or low-quality documents to the context can **hurt accuracy rather than help it**.
> 
> Key points:
> 
> 1. **Noise and distraction**: When retrieved documents are not relevant to the query, they introduce noise that can mislead the generator, causing it to produce incorrect or off-topic answers, which degrades answer accuracy and faithfulness.
> 
> 2. **Contradiction and confusion**: Irrelevant passages may conflict with the model's parametric knowledge or with each other, making it harder for the generator to decide what to rely on.
> 
> 3. **"Lost in the middle" / distraction effects**: Longer contexts filled with unhelpful documents dilute the useful evidence; models may attend to noise instead of the relevant passage.
> 
> 4. **Motivation for better retrieval**: This problem motivates the survey's discussion of improving RAG through better retrieval (e.g., optimizing retrieval sources, embedding models, hybrid search), advanced RAG techniques (reranking, filtering), and modular/adaptive RAG approaches (e.g., deciding when to retrieve at all, or filtering out low-relevance content before generation).
> 
> In short: irrelevant documents in the context typically **reduce** RAG accuracy, so quality and relevance of retrieved content matter more than simply retrieving more documents.

#### rag — 2/2

> According to the survey, adding irrelevant documents can unexpectedly **increase** accuracy by over 30%, contradicting the initial assumption that it would reduce quality [1]. The authors note this underscores the need for specialized strategies integrating retrieval with language generation and further research into RAG robustness [1].

#### mcp — 2/2

_2 вызовов MCP, прочитано 6946 симв., разделы: VI. TASK AND EVALUATION > C. Evaluation Aspects; VI. TASK AND EVALUATION > D. Evaluation Benchmarks and Tools; VII. DISCUSSION AND FUTURE PROSPECTS; VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context; VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness; VII. DISCUSSION AND FUTURE PROSPECTS > C. Hybrid Approaches_

> According to the survey (Section VII.B, "RAG Robustness," p. 14 of rag-survey-2312.10997.pdf), citing Cuconasu et al.: including irrelevant documents in the context can **unexpectedly increase accuracy by over 30%**, contradicting the initial assumption that irrelevant/noisy documents would reduce answer quality.
> 
> The survey notes this is surprising, since noise or contradictory information is generally expected to harm RAG output ("misinformation can be worse than no information at all"), and it underscores the need for further research and specialized strategies for integrating retrieval with generation models.

### 10. How large a context can current LLMs handle according to the survey, and why is RAG still needed then?

**Ожидание:** Over 200,000 tokens; RAG is still needed because a huge context slows inference (chunked retrieval / on-demand input is more efficient) and RAG quickly locates original references so users can verify answers (the process is observable, long context is a black box).

**Источник:** RAG vs Long Context · **проверяемые группы:** `200,000|200 000|200000|200k|200 k`, `speed|slow|efficien|latency|cost`, `verif|referenc|traceab|observab|black box`

**Найдено RAG:**
1. VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context, стр. 14 · cos 0.9948
2. II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · cos 0.9915
3. VII. DISCUSSION AND FUTURE PROSPECTS > D. Scaling laws of RAG, стр. 15 · cos 0.9907
4. II. OVERVIEW OF RAG, стр. 2 · cos 0.9906

#### plain — 1/3, нет: `200,000|200 000|200000|200k|200 k`, `verif|referenc|traceab|observab|black box`

> According to the survey, recent LLMs have greatly expanded context windows and can now handle very long inputs (with some models supporting contexts on the order of tens or even hundreds of thousands of tokens, and the survey notes progress toward million-token contexts). This raises a natural question: with such long contexts, is RAG still necessary?
> 
> The survey argues that RAG remains important for several reasons:
> 
> 1. **Cost and efficiency** — Feeding massive contexts into an LLM for every query is expensive and slow. Retrieval lets you fetch only the few most relevant chunks instead of stuffing everything into the prompt.
> 
> 2. **"Lost in the middle" and limited effective use of context** — Even with long contexts, models struggle to reliably use information buried in the middle of very long inputs; performance degrades as context length grows. RAG provides focused, relevant evidence.
> 
> 3. **Timeliness** — Long context windows don't solve the problem that model knowledge is frozen at training time. Retrieval supplies up-to-date information without retraining.
> 
> 4. **Privacy and domain data** — RAG lets models access private, proprietary, or domain-specific knowledge bases without putting all data into the model.
> 
> In short, long context and RAG are complementary: retrieval selects the right information, while the LLM reasons over it efficiently and accurately.

#### rag — 3/3

> According to the survey, current LLMs can effortlessly manage contexts exceeding 200,000 tokens [1]. Despite this, RAG remains irreplaceable for several reasons:
> 
> 1. **Efficiency**: Providing a large amount of context at once significantly impacts inference speed, while chunked retrieval and on-demand input improve operational efficiency [1].
> 2. **Verifiability/Interpretability**: RAG-based generation can quickly locate original references, helping users verify answers; the retrieval and reasoning process is observable, whereas long-context-only generation remains a black box [1].
> 3. **New opportunities**: Longer contexts actually benefit RAG, enabling it to tackle complex integrative or summary questions requiring reading large amounts of material — developing new RAG methods for super-long contexts is a future research trend [1].
> 
> Additionally, RAG excels at dynamic knowledge updates and real-time information access, which pretraining-based models lack [2], [4].

#### mcp — 3/3

_4 вызовов MCP, прочитано 11001 симв., разделы: VI. TASK AND EVALUATION > C. Evaluation Aspects; VI. TASK AND EVALUATION > D. Evaluation Benchmarks and Tools; VII. DISCUSSION AND FUTURE PROSPECTS; VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context; VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness; VII. DISCUSSION AND FUTURE PROSPECTS > C. Hybrid Approaches; VII. DISCUSSION AND FUTURE PROSPECTS > D. Scaling laws of RAG; VII. DISCUSSION AND FUTURE PROSPECTS > E. Production-Ready RAG_

> According to the survey (Section VII-A, "RAG vs Long Context," p. 14), current LLMs can effortlessly handle contexts exceeding **200,000 tokens** — enough to fit entire documents directly into the prompt.
> 
> Yet RAG remains necessary because:
> 
> 1. **Efficiency** — feeding a huge context at once significantly slows inference, while chunked retrieval and on-demand input greatly improve operational efficiency.
> 2. **Verifiability & observability** — RAG can quickly locate original references so users can verify answers, and the retrieval/reasoning process is observable, whereas pure long-context generation is a "black box."
> 
> The survey also notes long contexts are an opportunity: they let RAG tackle integrative and summary questions requiring much material, and developing RAG methods for super-long contexts is a future research trend (pp. 14–15).
