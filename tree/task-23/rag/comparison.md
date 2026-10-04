# Сравнение стратегий чанкинга

| метрика | fixed | structure |
|---|---|---|
| параметры | `{"norm":"minmax-0-1","overlap":200,"size":1000}` | `{"norm":"minmax-0-1","struct_max":4000}` |
| модель / dim | nomic-embed-text / 768 | nomic-embed-text / 768 |
| чанков | 1368 | 314 |
| символов min / медиана / среднее / max | 358 / 997 / 993 / 1000 | 18 / 3845 / 3481 / 3998 |
| мелких (< 200 симв.) | 0 (0%) | 1 (0%) |
| избыточность (Σ чанков / текст) | 1.24× | 1.00× |
| пересекают границу раздела | 50 (4%) | 0 (0%) |
| обрываются посреди предложения | 1208 (88%) | 14 (4%) |
| компоненты векторов min / max (нормализация min-max) | 0.000 / 1.000 | 0.000 / 1.000 |
| время эмбеддингов | 136566 мс (100 мс/чанк) | 123466 мс (393 мс/чанк) |
| поиск: hit@1 (чанк задевает раздел) | 10/12 | 9/12 |
| поиск: hit@1 (чанк целиком в разделе) | 2/12 | 9/12 |
| поиск: hit@3 | 11/12 | 11/12 |
| поиск: MRR | 0.87 | 0.83 |

## Пробные вопросы (docs/questions.json)

Ранг — позиция первого чанка из ожидаемого раздела (0 — нет в выдаче).

| вопрос | ожидаемый раздел | fixed: ранг / cos / top-1 | structure: ранг / cos / top-1 |
|---|---|---|---|
| What are the three paradigms of retrieval-augmented generation? | II. OVERVIEW OF RAG | 12 / 0.992 / REFERENCES | 6 / 0.992 / Front matter |
| What are the drawbacks of Naive RAG? | Naive RAG | 1 / 0.990 / II. OVERVIEW OF RAG > A. Naive RAG | 3 / 0.988 / VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness |
| When should I use RAG instead of fine-tuning the model? | RAG vs Fine-tuning | 1 / 0.994 / II. OVERVIEW OF RAG > D. RAG vs Fine-tuning | 1 / 0.995 / II. OVERVIEW OF RAG > D. RAG vs Fine-tuning |
| How can the user query be rewritten or expanded before retrieval? | Query Optimization | 3 / 0.990 / Front matter | 1 / 0.990 / III. RETRIEVAL > C. Query Optimization (part 1/2) |
| How are chunk size and metadata used to optimize the index? | Indexing Optimization | 1 / 0.991 / III. RETRIEVAL > A. Retrieval Source … III. RETRIEVAL > B. Indexing Optimization | 1 / 0.989 / III. RETRIEVAL > B. Indexing Optimization |
| How are embedding models fine-tuned for retrieval? | D. Embedding | 1 / 0.994 / III. RETRIEVAL > C. Query Optimization … III. RETRIEVAL > D. Embedding | 1 / 0.994 / III. RETRIEVAL > D. Embedding |
| How does iterative retrieval alternate between retrieving and generating? | Iterative Retrieval | 1 / 0.993 / V. AUGMENTATION PROCESS IN RAG … V. AUGMENTATION PROCESS IN RAG > A. Iterative Retrieval | 1 / 0.993 / V. AUGMENTATION PROCESS IN RAG > A. Iterative Retrieval |
| How does the model decide by itself when to retrieve? | Adaptive Retrieval | 1 / 0.987 / V. AUGMENTATION PROCESS IN RAG > C. Adaptive Retrieval … VI. TASK AND EVALUATION | 2 / 0.986 / III. RETRIEVAL > E. Adapter |
| Which benchmarks and tools are used to evaluate RAG systems? | Evaluation | 1 / 0.995 / VI. TASK AND EVALUATION > C. Evaluation Aspects … VII. DISCUSSION AND FUTURE PROSPECTS | 1 / 0.996 / VI. TASK AND EVALUATION > D. Evaluation Benchmarks and Tools |
| Do long-context LLMs make RAG unnecessary? | RAG vs Long Context | 1 / 0.992 / VII. DISCUSSION AND FUTURE PROSPECTS … VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context | 1 / 0.993 / VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context |
| How robust is RAG to noisy or irrelevant retrieved documents? | RAG Robustness | 1 / 0.993 / VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context … VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness | 1 / 0.995 / VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness |
| Can RAG work with images, audio and video? | Multi-modal RAG | 1 / 0.992 / VII. DISCUSSION AND FUTURE PROSPECTS > E. Production-Ready RAG … VII. DISCUSSION AND FUTURE PROSPECTS > F. Multi-modal RAG | 1 / 0.991 / VII. DISCUSSION AND FUTURE PROSPECTS > F. Multi-modal RAG |
