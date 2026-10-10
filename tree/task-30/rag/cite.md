# RAG: источники, цитаты и «не знаю» (задача 24)

Модель `glm-5.3-flash`, индекс `rag/index.sqlite` (стратегия `fixed`, 1368 чанков), второй этап: sim z≥2.5 + llm ≥5 · 20→4, «не знаю» при llm < 7 (без реранкера — z < 3.0).

Каждый ответ — JSON с `answer`, `sources` (source + section + chunk_id) и `quotes`. Проверяется кодом: источник указывает на фрагмент из контекста и chunk_id совпадает; цитата дословно есть в этом фрагменте; числа ответа есть в цитатах. Смысл ответа против цитат — отдельный вызов LLM при temperature 0, который видит только ответ и цитаты.

## Сводка

| вопросы | ответил | источники есть | цитаты есть | все цитаты дословно | числа ответа в цитатах | смысл = цитаты (судья: да) | всё сразу | «не знаю» | из них порогом | выдумал ответ |
|---|---|---|---|---|---|---|---|---|---|---|
| с ответом в корпусе (10) | 10 | 10/10 | 10/10 | 9/10 | 9/10 | 9/10 | 8/10 | 0 | 0 | — |
| без ответа в корпусе (5) | 0 | 0/0 | 0/0 | 0/0 | 0/0 | 0/0 | 0/0 | 5 | 5 | 0 |
| расплывчатые (3) | 0 | 0/0 | 0/0 | 0/0 | 0/0 | 0/0 | 0/0 | 3 | 3 | 0 |

## По вопросам

| # | вопрос | итог | ожидание | попыток | tok ответа | tok этапов |
|---|---|---|---|---|---|---|
| 1 | What randomized election timeout range does Raft recommend, and why are the timeouts randomized? | ✓ источники 3/3 · ✓ цитаты 5/5 дословно · ✓ числа 2/2 в цитатах · ✓ смысл по цитатам: да | 3/3 | 2 | 4695 | 3425 |
| 3 | What were the masses of the two black holes in GW150914, the mass of the final black hole, and how much mass was radiated as gravitational waves? | ✓ источники 1/1 · ✗ цитаты 2/3 дословно · ✗ числа 3/4 в цитатах · ✓ смысл по цитатам: да | 4/4 | 2 | 3158 | 6100 |
| 5 | How many positions per second does AlphaZero search in chess and shogi, compared with Stockfish and Elmo? | ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✓ числа 4/4 в цитатах · ✓ смысл по цитатам: да | 4/4 | 1 | 1915 | 6192 |
| 7 | What does NISQ stand for, and roughly how many gates can such a device execute before noise overwhelms the signal? | ✓ источники 2/2 · ✓ цитаты 2/2 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да | 3/3 | 1 | 1627 | 2227 |
| 9 | For what kind of optimization problems is Bayesian optimization best suited, according to the tutorial? | ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да | 2/3 | 2 | 4235 | 5855 |
| 11 | What share of the US workforce could have at least 10% of their work tasks affected by LLMs, and what share at least 50%? | ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✓ числа 3/3 в цитатах · ✓ смысл по цитатам: да | 2/2 | 1 | 1843 | 4960 |
| 14 | What share of global greenhouse gas emissions comes from cement and steel production? | ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✓ числа 1/1 в цитатах · ✗ смысл по цитатам: частично | 1/1 | 2 | 1857 | 393 |
| 15 | Which language models and which synthetic task were used in the study showing that models get lost in the middle of long contexts? | ✓ источники 2/2 · ✓ цитаты 2/2 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да | 5/5 | 1 | 1985 | 4408 |
| 17 | According to the RAG survey, how does adding irrelevant documents to the context affect the accuracy of RAG? | ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да | 2/2 | 1 | 945 | 5399 |
| 20 | Can a Spectre attack be mounted from a web page? | ✓ источники 1/1 · ✓ цитаты 2/2 дословно · ✓ смысл по цитатам: да | 2/2 | 1 | 1600 | 3030 |
| 22 | What is the recommended ibuprofen dose for a five-year-old child? | не знаю — порог релевантности, модель не вызывалась | — | 0 | 0 | 512 |
| 23 | What block time does Bitcoin target, and how is its mining difficulty adjusted? | не знаю — порог релевантности, модель не вызывалась | — | 0 | 0 | 0 |
| 24 | Which national team won the 2018 FIFA World Cup? | не знаю — порог релевантности, модель не вызывалась | — | 0 | 0 | 0 |
| 25 | How many qubits did Google's Sycamore processor use in its 2019 quantum supremacy experiment? | не знаю — порог релевантности, модель не вызывалась | — | 0 | 0 | 1121 |
| 26 | What was the signal-to-noise ratio of the binary neutron star merger GW170817? | не знаю — порог релевантности, модель не вызывалась | — | 0 | 0 | 4635 |
| v1 | How fast is it? | не знаю — порог релевантности, модель не вызывалась | — | 0 | 0 | 0 |
| v2 | What was the final result? | не знаю — порог релевантности, модель не вызывалась | — | 0 | 0 | 0 |
| v3 | Which method works best? | не знаю — порог релевантности, модель не вызывалась | — | 0 | 0 | 0 |

## Ответы

### 1. What randomized election timeout range does Raft recommend, and why are the timeouts randomized?

**Ожидание:** Election timeouts are chosen randomly from a fixed interval such as 150–300 ms, so that split votes are rare and resolved quickly (usually a single server times out first and wins).

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 4 чанк._

1. #1 cos 0.9960 z 3.74 llm 10 · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.
2. #2 cos 0.9948 z 3.35 llm 10 · raft-consensus.pdf — Front matter, стр. 15 · 996 симв.
3. #3 cos 0.9946 z 3.31 llm 7 · raft-consensus.pdf — Front matter, стр. 10–11 · 1000 симв.
4. #4 cos 0.9939 z 3.09 llm 7 · raft-consensus.pdf — Front matter, стр. 15 · 994 симв.

Повторы: судья: не в цитатах — candidates restart their randomized timeout before retrying, reducing the chance of another split vote; usually only one server times out first, wins, and sends heartbeats before others time out

> Raft recommends a conservative election timeout of 150–300ms [2]. Timeouts are chosen randomly from this fixed interval to prevent split votes: randomization spreads out servers so usually only one times out first, wins, and sends heartbeats before others time out, and candidates restart their randomized timeout before retrying, reducing the chance of another split vote [1]. Randomization also makes split votes unlikely given the timing requirement (broadcast time an order of magnitude below the election timeout) [3].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 6 · fixed-raft-consensus-0034
>   [2] raft-consensus.pdf — Front matter, стр. 15 · fixed-raft-consensus-0092
>   [3] raft-consensus.pdf — Front matter, стр. 10–11 · fixed-raft-consensus-0063
> 
> Цитаты:
>   [1] «To prevent split votes in the first place, election timeouts are chosen randomly from a fixed interval (e.g., 150–300ms).»
>   [1] «This spreads out the servers so that in most cases only a single server will time out; it wins the election and sends heartbeats before any other servers time out.»
>   [1] «Each candidate restarts its randomized election timeout at the start of an election, and it waits for that timeout to elapse before starting the next election; this reduces the likelihood of another split vote in the new election.»
>   [2] «We recommend using a conservative election timeout such as 150–300ms; such timeouts are unlikely to cause unnecessary leader changes and will still provide good availability.»
>   [3] «given the randomized approach used for election timeouts, this inequality also makes split votes unlikely.»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 5/5 дословно · ✓ числа 2/2 в цитатах · ✓ смысл по цитатам: да


### 3. What were the masses of the two black holes in GW150914, the mass of the final black hole, and how much mass was radiated as gravitational waves?

**Ожидание:** Initial black holes of 36 and 29 solar masses, a final black hole of 62 solar masses, and about 3.0 solar masses radiated in gravitational waves.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 2 чанк._

1. #3 cos 0.9876 z 4.25 llm 10 · gw150914-ligo-1602.03837.pdf — Front matter, стр. 1 · 994 симв.
2. #4 cos 0.9872 z 4.12 llm 7 · gw150914-ligo-1602.03837.pdf — V. SEARCHES > B. Binary coalescence search, стр. 7 · 993 симв.

Повторы: quote [1] «the initial black hole masses are 36−4 M⊙ and 29−4 M ⊙ , and the final black hole mass is 62þ4 −4 M ⊙ , with 3.0−0.5 M ⊙ c radiated in gravitational waves.» is not in any excerpt — copy it character for character; numbers 36, 29, 62, 30 of the answer are in none of the quotes — quote where they come from or drop them

> The initial black holes had masses of 36 M☉ and 29 M☉, the final black hole had a mass of 62 M☉, and 3.0 M☉ c² was radiated in gravitational waves [1].
> 
> Источники:
>   [1] gw150914-ligo-1602.03837.pdf — Front matter, стр. 1 · fixed-gw150914-ligo-1602.03837-0001
> 
> Цитаты:
>   [1] «the initial black hole masses are 36−4 M⊙ and 29−4 M ⊙ , and the final black hole mass is»
>   [1] «62þ4»
>   [1] «−4 M ⊙ , with 3.0−0.5 M ⊙ c radiated in gravitational waves.»
> 
> Проверка: ✓ источники 1/1 · ✗ цитаты 2/3 дословно · ✗ числа 3/4 в цитатах · ✓ смысл по цитатам: да

- ✗ цитата [1] не найдена дословно (Short)

### 5. How many positions per second does AlphaZero search in chess and shogi, compared with Stockfish and Elmo?

**Ожидание:** AlphaZero searches about 80 thousand positions per second in chess and 40 thousand in shogi, versus 70 million for Stockfish and 35 million for Elmo.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 3 чанк._

1. #1 cos 0.9978 z 4.69 llm 10 · alphazero-chess-shogi-1712.01815.pdf — Front matter, стр. 5 · 999 симв.
2. #2 cos 0.9958 z 4.05 llm 10 · alphazero-chess-shogi-1712.01815.pdf — Front matter, стр. 5 · 999 симв.
3. #4 cos 0.9953 z 3.90 llm 10 · alphazero-chess-shogi-1712.01815.pdf — Front matter, стр. 18–19 · 909 симв.

> AlphaZero searches 80 thousand positions per second in chess and 40 thousand in shogi, compared to 70 million for Stockfish and 35 million for Elmo [1].
> 
> Источники:
>   [1] alphazero-chess-shogi-1712.01815.pdf — Front matter, стр. 5 · fixed-alphazero-chess-shogi-1712.01815-0016
> 
> Цитаты:
>   [1] «AlphaZero searches just 80 thousand positions per second in chess and 40 thousand in shogi, compared to 70 million for Stockfish and 35 million for Elmo.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✓ числа 4/4 в цитатах · ✓ смысл по цитатам: да


### 7. What does NISQ stand for, and roughly how many gates can such a device execute before noise overwhelms the signal?

**Ожидание:** Noisy Intermediate-Scale Quantum (about 50–100 qubits); circuits of not many more than about 1000 two-qubit gates before noise overwhelms the signal.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 3 чанк._

1. #3 cos 0.9909 z 3.26 llm 10 · nisq-preskill-1801.00862.pdf — Front matter, стр. 5 · 998 симв.
2. #2 cos 0.9909 z 3.28 llm 7 · nisq-preskill-1801.00862.pdf — Front matter, стр. 4–5 · 1000 симв.
3. #6 cos 0.9900 z 2.95 llm 7 · nisq-preskill-1801.00862.pdf — Front matter, стр. 4 · 999 симв.

> NISQ stands for Noisy Intermediate-Scale Quantum [2]. Such noisy devices are expected to execute circuits with no more than about 1000 gates (1000 fundamental two-qubit operations) before noise overwhelms the signal [1].
> 
> Источники:
>   [1] nisq-preskill-1801.00862.pdf — Front matter, стр. 5 · fixed-nisq-preskill-1801.00862-0019
>   [2] nisq-preskill-1801.00862.pdf — Front matter, стр. 4–5 · fixed-nisq-preskill-1801.00862-0017
> 
> Цитаты:
>   [2] «This stands for Noisy IntermediateScale Quantum»
>   [1] «we don’t expect to be able to execute a circuit that contains many more than about 1000 gates — that is,
> 1000 fundamental two-qubit operations — because the noise will overwhelm the signal»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 2/2 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да


### 9. For what kind of optimization problems is Bayesian optimization best suited, according to the tutorial?

**Ожидание:** Objective functions that are expensive to evaluate (minutes or hours), over continuous domains of less than 20 dimensions, tolerating stochastic noise in evaluations.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 3 чанк._

1. #1 cos 0.9912 z 3.92 llm 10 · bayesian-optimization-tutorial-1807.02811.pdf — Front matter, стр. 1 · 991 симв.
2. #9 cos 0.9891 z 3.24 llm 10 · bayesian-optimization-tutorial-1807.02811.pdf — Front matter, стр. 1 · 999 симв.
3. #3 cos 0.9904 z 3.68 llm 5 · bayesian-optimization-tutorial-1807.02811.pdf — Front matter, стр. 12 · 998 симв.

Повторы: судья: не в цитатах — objective functions take minutes or hours to evaluate; simple feasible set (e.g., a hyper-rectangle or simplex) where membership is easy to assess; a continuous objective

> Bayesian optimization is best suited for optimization over continuous domains of fewer than 20 dimensions, with a simple feasible set (such as a hyper-rectangle) where membership is easy to assess, a continuous objective function, and it tolerates stochastic noise in function evaluations [1][2].
> 
> Источники:
>   [1] bayesian-optimization-tutorial-1807.02811.pdf — Front matter, стр. 1 · fixed-bayesian-optimization-tutorial-1807.02811-0000
>   [2] bayesian-optimization-tutorial-1807.02811.pdf — Front matter, стр. 1 · fixed-bayesian-optimization-tutorial-1807.02811-0002
> 
> Цитаты:
>   [1] «It is best-suited for optimization over continuous domains of less than 20 dimensions, and tolerates stochastic noise in function evaluations.»
>   [2] «The feasible set A is a simple set, in which it is easy to assess membership.»
>   [2] «The objective function f is continuous.»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да


### 11. What share of the US workforce could have at least 10% of their work tasks affected by LLMs, and what share at least 50%?

**Ожидание:** Around 80% of the U.S. workforce could have at least 10% of their tasks affected; about 19% of workers may see at least 50% of their tasks impacted.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 4 чанк._

1. #2 cos 0.9930 z 3.62 llm 10 · gpts-are-gpts-labor-2303.10130.pdf — Front matter, стр. 11 · 994 симв.
2. #1 cos 0.9935 z 3.77 llm 8 · gpts-are-gpts-labor-2303.10130.pdf — Front matter, стр. 1 · 991 симв.
3. #4 cos 0.9921 z 3.30 llm 6 · gpts-are-gpts-labor-2303.10130.pdf — Front matter, стр. 23 · 999 симв.
4. #7 cos 0.9913 z 3.02 llm 6 · gpts-are-gpts-labor-2303.10130.pdf — Front matter, стр. 3 · 999 симв.

> Based on the β values, 80% of US workers are in occupations with at least 10% of tasks exposed to LLMs, while 19% are in occupations where over half of tasks are exposed [1].
> 
> Источники:
>   [1] gpts-are-gpts-labor-2303.10130.pdf — Front matter, стр. 11 · fixed-gpts-are-gpts-labor-2303.10130-0047
> 
> Цитаты:
>   [1] «Based on the 𝛽 values, we estimate that 80% of workers belong to an occupation with at least 10% of its tasks exposed to LLMs, while 19% of workers are in an occupation where over half of its tasks are labeled as exposed.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✓ числа 3/3 в цитатах · ✓ смысл по цитатам: да


### 14. What share of global greenhouse gas emissions comes from cement and steel production?

**Ожидание:** Cement and steel production together account for over 10% of all global GHG emissions.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 1 чанк._

1. #1 cos 0.9924 z 3.00 llm 7 · climate-change-ml-1906.05433.pdf — Front matter, стр. 27–28 · 998 симв.

Повторы: судья: не в цитатах — Cement is part of the over-10% share (quote only mentions steel production); после повтора судьи формат сломался — оставлен предыдущий ответ

> Cement and steel production together account for over 10% of all global GHG emissions [1].
> 
> Источники:
>   [1] climate-change-ml-1906.05433.pdf — Front matter, стр. 27–28 · fixed-climate-change-ml-1906.05433-0114
> 
> Цитаты:
>   [1] «steel production together account for over 10% of all global GHG emissions [378]»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✓ числа 1/1 в цитатах · ✗ смысл по цитатам: частично

- судья: не подтверждено цитатами — Cement is part of the over-10% share (quote only mentions steel production)

### 15. Which language models and which synthetic task were used in the study showing that models get lost in the middle of long contexts?

**Ожидание:** Open MPT-30B-Instruct and LongChat-13B (16K), closed GPT-3.5-Turbo and Claude-1.3; besides multi-document QA they used a synthetic key-value retrieval task.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 4 чанк._

1. #4 cos 0.9906 z 2.89 llm 8 · lost-in-the-middle-2307.03172.pdf — Front matter, стр. 2 · 998 симв.
2. #5 cos 0.9904 z 2.81 llm 8 · lost-in-the-middle-2307.03172.pdf — Front matter, стр. 2 · 999 симв.
3. #12 cos 0.9897 z 2.57 llm 8 · lost-in-the-middle-2307.03172.pdf — Front matter, стр. 1–2 · 996 симв.
4. #2 cos 0.9914 z 3.15 llm 7 · lost-in-the-middle-2307.03172.pdf — Front matter, стр. 1 · 998 симв.

> The study used open models MPT-30B-Instruct and LongChat-13B (16K), and closed models OpenAI's GPT-3.5-Turbo and Anthropic's Claude-1.3, and studied a synthetic key-value retrieval task where models must return the value associated with a specific key in JSON-formatted key-value pairs [3][1].
> 
> Источники:
>   [1] lost-in-the-middle-2307.03172.pdf — Front matter, стр. 2 · fixed-lost-in-the-middle-2307.03172-0008
>   [3] lost-in-the-middle-2307.03172.pdf — Front matter, стр. 1–2 · fixed-lost-in-the-middle-2307.03172-0004
> 
> Цитаты:
>   [3] «open (MPT-30B-Instruct, LongChat-13B
> (16K)) and closed (OpenAI’s GPT-3.5-Turbo and
> Anthropic’s Claude-1.3) language models»
>   [1] «models
> are given a collection of JSON-formatted key-value
> pairs and must return the value associated with a
> specific key»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 2/2 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да


### 17. According to the RAG survey, how does adding irrelevant documents to the context affect the accuracy of RAG?

**Ожидание:** Unexpectedly, including irrelevant documents can increase accuracy by over 30%, contradicting the assumption that it reduces quality.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 1 чанк._

1. #1 cos 0.9927 z 4.32 llm 10 · rag-survey-2312.10997.pdf — VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context, стр. 14 · 998 симв.

> Unexpectedly, including irrelevant documents can increase accuracy by over 30%, contradicting the assumption that quality would decrease [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — VII. DISCUSSION AND FUTURE PROSPECTS > A. RAG vs Long Context, стр. 14 · fixed-rag-survey-2312.10997-0081
> 
> Цитаты:
>   [1] «The research findings reveal that including irrelevant documents can unexpectedly increase accuracy by over 30%, contradicting the initial assumption of reduced quality.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да


### 20. Can a Spectre attack be mounted from a web page?

**Ожидание:** Yes: the authors wrote a JavaScript program that, run in the browser (Chrome), reads private memory of the browser process — violating browser sandboxing.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 3 чанк._

1. #5 cos 0.9906 z 2.95 llm 10 · spectre-attacks-1801.01203.pdf — Front matter, стр. 2 · 994 симв.
2. #4 cos 0.9909 z 3.04 llm 7 · spectre-attacks-1801.01203.pdf — Front matter, стр. 2 · 995 симв.
3. #9 cos 0.9897 z 2.64 llm 7 · spectre-attacks-1801.01203.pdf — Front matter, стр. 6–7 · 997 симв.

> Yes. Spectre attacks can be mounted via portable JavaScript code from a browser, violating browser sandboxing; the authors wrote a JavaScript program that successfully reads data from the address space of the browser process running it [1].
> 
> Источники:
>   [1] spectre-attacks-1801.01203.pdf — Front matter, стр. 2 · fixed-spectre-attacks-1801.01203-0010
> 
> Цитаты:
>   [1] «Spectre attacks can also be used to violate browser sandboxing, by mounting them via portable JavaScript code.»
>   [1] «We wrote a JavaScript program that successfully reads data from the address space of the browser process running it.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 2/2 дословно · ✓ смысл по цитатам: да


### 22. What is the recommended ibuprofen dose for a five-year-old child?

**Ожидание:** Not in the corpus — the answer should say the documents do not cover it.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 0 чанк._

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — llm 0))
> Уточните, пожалуйста: о каком документе или системе вопрос «What is the recommended ibuprofen dose for a five-year-old child?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: lost-in-the-middle-2307.03172.pdf — Front matter, стр. 16–17 (llm 0)
>   ближе всего: gw150914-ligo-1602.03837.pdf — V. SEARCHES > A. Generic transient search, стр. 6 (z 2.19)
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 (z 2.11)


### 23. What block time does Bitcoin target, and how is its mining difficulty adjusted?

**Ожидание:** Not in the corpus (Raft and the other consensus text never describe Bitcoin) — the answer should say the documents do not cover it.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 0 чанк._

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — z 2.30))
> Уточните, пожалуйста: о каком документе или системе вопрос «What block time does Bitcoin target, and how is its mining difficulty adjusted?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: gw150914-ligo-1602.03837.pdf — V. SEARCHES > B. Binary coalescence search, стр. 7 (z 2.30)
>   ближе всего: lost-in-the-middle-2307.03172.pdf — Front matter, стр. 9 (z 2.01)
>   ближе всего: gw150914-ligo-1602.03837.pdf — III. DETECTORS, стр. 5 (z 1.99)


### 24. Which national team won the 2018 FIFA World Cup?

**Ожидание:** Not in the corpus — the answer should say the documents do not cover it.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 0 чанк._

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — z 2.31))
> Уточните, пожалуйста: о каком документе или системе вопрос «Which national team won the 2018 FIFA World Cup?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: alphazero-chess-shogi-1712.01815.pdf — Front matter, стр. 11 (z 2.31)
>   ближе всего: alphazero-chess-shogi-1712.01815.pdf — Front matter, стр. 2 (z 2.04)
>   ближе всего: alphazero-chess-shogi-1712.01815.pdf — Front matter, стр. 6–7 (z 2.01)


### 25. How many qubits did Google's Sycamore processor use in its 2019 quantum supremacy experiment?

**Ожидание:** Not in the corpus (Preskill's NISQ essay predates Sycamore) — the answer should say the documents do not cover it.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 0 чанк._

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — llm 4))
> Уточните, пожалуйста: о каком документе или системе вопрос «How many qubits did Google's Sycamore processor use in its 2019 quantum supremacy experiment?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: nisq-preskill-1801.00862.pdf — Front matter, стр. 5 (llm 4)
>   ближе всего: nisq-preskill-1801.00862.pdf — Front matter, стр. 17 (llm 0)
>   ближе всего: nisq-preskill-1801.00862.pdf — Front matter, стр. 7 (llm 0)


### 26. What was the signal-to-noise ratio of the binary neutron star merger GW170817?

**Ожидание:** Not in the corpus (the LIGO paper is about GW150914, a black hole merger) — the answer should say the documents do not cover it.

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 0 чанк._

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — llm 4))
> Уточните, пожалуйста: о каком документе или системе вопрос «What was the signal-to-noise ratio of the binary neutron star merger GW170817?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: gw150914-ligo-1602.03837.pdf — II. OBSERVATION, стр. 2 (llm 4)
>   ближе всего: gw150914-ligo-1602.03837.pdf — V. SEARCHES > B. Binary coalescence search, стр. 7 (llm 4)
>   ближе всего: gw150914-ligo-1602.03837.pdf — II. OBSERVATION, стр. 2–3 (llm 4)


### v1. How fast is it?

**Ожидание:** не ясно, о каком документе речь: «не знаю» и просьба уточнить

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 0 чанк._

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — z 2.30))
> Уточните, пожалуйста: о каком документе или системе вопрос «How fast is it?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: raft-consensus.pdf — Front matter, стр. 15 (z 2.30)
>   ближе всего: gw150914-ligo-1602.03837.pdf — V. SEARCHES > B. Binary coalescence search, стр. 7 (z 2.28)
>   ближе всего: gw150914-ligo-1602.03837.pdf — V. SEARCHES > A. Generic transient search, стр. 6 (z 2.22)


### v2. What was the final result?

**Ожидание:** не ясно, о каком документе речь: «не знаю» и просьба уточнить

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 0 чанк._

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — z 2.37))
> Уточните, пожалуйста: о каком документе или системе вопрос «What was the final result?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: raft-consensus.pdf — Front matter, стр. 15 (z 2.37)
>   ближе всего: raft-consensus.pdf — Front matter, стр. 4 (z 2.34)
>   ближе всего: alphazero-chess-shogi-1712.01815.pdf — Front matter, стр. 11 (z 2.33)


### v3. Which method works best?

**Ожидание:** не ясно, о каком документе речь: «не знаю» и просьба уточнить

_sim z≥2.5 + llm ≥5 · 20→4 · в контексте 0 чанк._

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — z 2.11))
> Уточните, пожалуйста: о каком документе или системе вопрос «Which method works best?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 (z 2.11)
>   ближе всего: rag-survey-2312.10997.pdf — IV. GENERATION > A. Context Curation, стр. 10 (z 2.10)
>   ближе всего: raft-consensus.pdf — Front matter, стр. 1 (z 2.02)

