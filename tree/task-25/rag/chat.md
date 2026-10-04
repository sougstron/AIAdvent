# RAG-чат с памятью задачи (задача 25)

Модель `glm-5.3-flash`, индекс `rag/index.sqlite` (стратегия `fixed`), память задачи on. Каждый ход: вопрос (+ строка состояния в запросе эмбеддинга) → retrieval → ответ с обязательными источниками и дословными цитатами → экстрактор обновляет память задачи (цель / уточнено / ограничения / термины), которая блоком `<task-state>` едет в каждый следующий запрос.

## Сводка

| сценарий | ходов | ответов с источниками | цитаты дословно | числа в цитатах | судья: да | «не знаю» правильно | ожидание must | выдумал | память: проверок прошло | цель поймана |
|---|---|---|---|---|---|---|---|---|---|---|
| 1. Сервис конфигураций на Raft (raft-consensus.pdf) | 14 | 13/13 | 13/13 | 11/13 | 7/13 | 1/1 | 22/23 | 0 | 9/9 | 2/2 |
| 2. Production-чат по документации компании (rag-survey-2312.10997.pdf) | 13 | 9/10 | 9/10 | 9/10 | 7/10 | 1/1 | 14/21 | 0 | 6/8 | 2/2 |

## Сценарий 1: Сервис конфигураций на Raft (raft-consensus.pdf)

Ожидаемая цель: спроектировать по статье о Raft внутренний сервис конфигураций: кластер из 5 узлов, строго актуальные чтения, смена состава без даунтайма

Цель в памяти после диалога: «Спроектировать внутренний распределённый сервис конфигураций на основе Raft, устойчивый к отказам узлов; начать с понимания устройства Raft.» (2/2)

Память в конце:

```json
{
  "goal": "Спроектировать внутренний распределённый сервис конфигураций на основе Raft, устойчивый к отказам узлов; начать с понимания устройства Raft.",
  "clarified": [
    "Проектируется внутренний сервис конфигураций",
    "Должен быть распределённым и переживать отказ узлов",
    "Основой служит статья о Raft",
    "Изучен механизм выборов лидера в Raft",
    "Изучены требования к таймаутам выборов",
    "Изучена причина рандомизации таймаутов выборов",
    "Договорились: quorum = 3",
    "Изучен механизм коммита записей лидером"
  ],
  "constraints": [
    "Кластер ровно из 5 узлов",
    "Строго актуальные (линеаризуемые) чтения",
    "Смена состава кластера без даунтайма"
  ],
  "terms": [
    {
      "term": "Raft",
      "meaning": "Алгоритм консенсуса для реплицируемого лога: выборы лидера, репликация лога, безопасность"
    },
    {
      "term": "Кворум",
      "meaning": "Большинство узлов кластера; в этом диалоге зафиксировано: quorum = 3"
    },
    {
      "term": "Election timeout",
      "meaning": "Случайный таймаут (рекоменд. 150–300 мс): фолловер без связи с лидером становится кандидатом"
    },
    {
      "term": "RequestVote RPC",
      "meaning": "Запрос голоса, который кандидат рассылает другим серверам на выборах"
    },
    {
      "term": "Broadcast time",
      "meaning": "Время рассылки RPC, должно быть на порядок меньше election timeout; типично 0,5–20 мс"
    },
    {
      "term": "MTBF",
      "meaning": "Среднее время между отказами; election timeout должен быть на порядки меньше MTBF"
    },
    {
      "term": "Split vote",
      "meaning": "Разделение голосов, когда никто не набрал кворум; разрешается благодаря рандомизации таймаутов"
    },
    {
      "term": "Committed запись",
      "meaning": "Запись считается committed, когда реплицирована на большинство; она durable и будет применена всеми state machines"
    },
    {
      "term": "Commit index",
      "meaning": "Высший известный committed-индекс; лидер передаёт его в AppendEntries, фолловеры применяют записи по порядку"
    },
    {
      "term": "Joint consensus",
      "meaning": "Двухфазное изменение конфигурации: переходная конфигурация (старая+новая, отдельные большинства), затем новая; без даунтайма"
    }
  ]
}
```

| # | вопрос | статус | источники | ожидание | память |
|---|---|---|---|---|---|
| 1 | Мы проектируем внутренний сервис конфигураций по статье о Raft: распределённый, переживает отказ узлов. С чего начать — как Raft вообще устроен, коротко? | answer (ждали answer) | 4/4 | 2/2 | ✓ |
| 2 | Ограничение фиксируем: кластер ровно из 5 узлов. Сколько узлов нужно для кворума и почему? | answer (ждали answer) | 3/3 | 2/2 | ✓ |
| 3 | Как выбирается лидер? Кто начинает выборы и как побеждает? | answer (ждали answer) | 2/2 | 1/2 | — |
| 4 | А таймауты? | answer (ждали answer) | 3/3 | 2/2 | — |
| 5 | Зачем таймауты выборов рандомизированы? | answer (ждали answer) | 1/1 | 1/1 | — |
| 6 | Согласуем термин: для нас «quorum = 3». Как лидер надёжно фиксирует запись в логе? | answer (ждали answer) | 1/1 | 2/2 | ✓ |
| 7 | Ещё требование: чтения должны быть строго актуальными, никаких устаревших значений. Что это меняет для нас по статье? | answer (ждали answer) | 2/2 | 2/2 | ✓ |
| 8 | Состав кластера менять придётся, но без даунтайма. Как Raft меняет конфигурацию? | answer (ждали answer) | 3/3 | 1/1 | ✓ |
| 9 | А новый узел, который только поднялся, — он сразу голосует? | answer (ждали answer) | 2/2 | 1/1 | — |
| 10 | Если узел отстал на очень много записей, как его наверстать? | answer (ждали answer) | 1/1 | 1/1 | — |
| 11 | Напомни, сколько у нас узлов в кластере и какой кворум? | answer (ждали answer) | 1/1 | 2/2 | — |
| 12 | Что статья о Raft говорит про шардирование PostgreSQL? | unknown (ждали unknown) | 0/0 | — | — |
| 13 | Как называется переходная конфигурация при смене состава кластера? | answer (ждали answer) | 1/1 | 1/1 | — |
| 14 | Собери итог списком: цель проекта и все зафиксированные ограничения и термины. | answer (ждали answer) | 4/4 | 4/4 | ✓ |

### Ответы

#### 1.01 Мы проектируем внутренний сервис конфигураций по статье о Raft: распределённый, переживает отказ узлов. С чего начать — как Raft вообще устроен, коротко?

_контекст:_
1. #1 cos 0.9943 z 3.71 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 3 · 994 симв.
2. #4 cos 0.9933 z 3.42 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 3 · 999 симв.
3. #2 cos 0.9936 z 3.51 llm 6 (rewrite) · raft-consensus.pdf — Front matter, стр. 1 · 998 симв.
4. #5 cos 0.9931 z 3.37 llm 6 (rewrite) · raft-consensus.pdf — Front matter, стр. 1 · 994 симв.

> Raft — алгоритм консенсуса для реплицируемого лога: сначала выбирается выделенный лидер, который принимает записи от клиентов, реплицирует их на другие серверы и решает, когда их безопасно применять. Задача разбита на три относительно независимые подзадачи: выборы лидера (при отказе существующего выбирается новый, с рандомизированными таймерами), репликация лога и безопасность. [1][2][3]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 3 · fixed-raft-consensus-0017
>   [2] raft-consensus.pdf — Front matter, стр. 3 · fixed-raft-consensus-0018
>   [3] raft-consensus.pdf — Front matter, стр. 1 · fixed-raft-consensus-0000
>   [4] raft-consensus.pdf — Front matter, стр. 1 · fixed-raft-consensus-0005
> 
> Цитаты:
>   [1] «Raft implements consensus by first electing a distinguished leader, then giving the leader complete responsibility for managing the replicated log. The leader accepts log entries from clients, replicates them on other servers, and tells servers when it is safe to apply log entries to their state machines.»
>   [2] «Raft decomposes the consensus problem into three relatively independent subproblems, which are discussed in the subsections that follow: • Leader election: a new leader must be chosen when an existing leader fails»
>   [3] «• Leader election: Raft uses randomized timers to elect leaders.»
>   [4] «including decomposition (Raft separates leader election, log replication, and safety)»
> 
> Проверка: ✓ источники 4/4 · ✓ цитаты 4/4 дословно · ✓ смысл по цитатам: да

#### 1.02 Ограничение фиксируем: кластер ровно из 5 узлов. Сколько узлов нужно для кворума и почему?

_контекст:_
1. #15 cos 0.9925 z 2.80 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 5 · 998 симв.
2. #11 cos 0.9927 z 2.86 llm 6 (rewrite) · raft-consensus.pdf — Front matter, стр. 5 · 999 симв.
3. #20 cos 0.9921 z 2.67 llm 6 (rewrite) · raft-consensus.pdf — Front matter, стр. 8 · 1000 симв.
4. #17 cos 0.9924 z 2.76 llm 5 (rewrite) · raft-consensus.pdf — Front matter, стр. 15–16 · 993 симв.

> Кластер из 5 узлов типичен и переживает отказ двух узлов; для кворума нужно большинство серверов — новые записи принимаются и применяются, пока большинство серверов работают, а лидером становится кандидат, получивший голоса большинства всего кластера [1][2][3].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 5 · fixed-raft-consensus-0029
>   [3] raft-consensus.pdf — Front matter, стр. 8 · fixed-raft-consensus-0045
>   [2] raft-consensus.pdf — Front matter, стр. 5 · fixed-raft-consensus-0026
> 
> Цитаты:
>   [1] «A Raft cluster contains several servers; five is a typical number, which allows the system to tolerate two failures.»
>   [2] «Raft can accept, replicate, and apply new log entries as long as a majority of the servers are up»
>   [3] «A candidate that receives votes from a majority of the full cluster becomes the new leader.»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 3/3 дословно (2 — из другого фрагмента) · ✓ числа 1/1 в цитатах · ✓ смысл по цитатам: да

#### 1.03 Как выбирается лидер? Кто начинает выборы и как побеждает?

_контекст:_
1. #3 cos 0.9955 z 3.66 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 5–6 · 999 симв.
2. #8 cos 0.9945 z 3.36 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.
3. #10 cos 0.9944 z 3.32 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 5 · 998 симв.
4. #13 cos 0.9941 z 3.23 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.

> Выборы запускает последователь: если фолловер долго не получает связи от лидера (election timeout), он переходит в состояние кандидата, голосует за себя и параллельно рассылает RequestVote RPC другим серверам [1]. Таймауты выбираются случайно из фиксированного интервала (например, 150–300 мс), поэтому обычно успевает только один сервер: он выигрывает выборы и рассылает сердцебиения раньше остальных [4]. [1][4]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 5–6 · fixed-raft-consensus-0031
>   [4] raft-consensus.pdf — Front matter, стр. 6 · fixed-raft-consensus-0034
> 
> Цитаты:
>   [1] «If a follower receives no communication over a period of time called the election timeout, then it assumes there is no viable leader and begins an election to choose a new leader.»
>   [1] «To begin an election, a follower increments its current term and transitions to candidate state. It then votes for itself and issues RequestVote RPCs in parallel to each of the other servers»
>   [4] «election timeouts are chosen randomly from a fixed interval (e.g., 150–300ms).»
>   [4] «it wins the election and sends heartbeats before any other servers time out»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 4/4 дословно · ✓ числа 2/2 в цитатах · ✓ смысл по цитатам: да

#### 1.04 А таймауты?

_контекст:_
1. #12 cos 0.9929 z 3.11 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 10–11 · 1000 симв.
2. #1 cos 0.9947 z 3.62 llm 9 (rewrite) · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.
3. #15 cos 0.9928 z 3.08 llm 9 (rewrite) · raft-consensus.pdf — Front matter, стр. 15 · 996 симв.
4. #6 cos 0.9936 z 3.31 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 5–6 · 999 симв.

> Таймауты выборов выбираются случайно из фиксированного интервала; рекомендован консервативный диапазон 150–300 мс [1][3]. Требования к таймингам: broadcast time должен быть на порядок меньше election timeout (чтобы сердцебиения успевали предотвращать выборы и split votes были редки), а election timeout — на порядки меньше MTBF; при типичном broadcast time 0,5–20 мс таймаут получается 10–500 мс [2].
> 
> Источники:
>   [2] raft-consensus.pdf — Front matter, стр. 6 · fixed-raft-consensus-0034
>   [1] raft-consensus.pdf — Front matter, стр. 10–11 · fixed-raft-consensus-0063
>   [3] raft-consensus.pdf — Front matter, стр. 15 · fixed-raft-consensus-0092
> 
> Цитаты:
>   [1] «To prevent split votes in the first place, election timeouts are chosen randomly from a fixed interval (e.g., 150–300ms).»
>   [2] «The election timeout should be a few orders of magnitude less than MTBF so that the system makes steady progress.»
>   [2] «the broadcast time may range from 0.5ms to 20ms, depending on storage technology. As a result, the election timeout is likely to be somewhere between 10ms and 500ms.»
>   [3] «We recommend using a conservative election timeout such as 150–300ms; such timeouts are unlikely to cause unnecessary leader changes and will still provide good availability.»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 4/4 дословно (3 — из другого фрагмента) · ✓ числа 6/6 в цитатах · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 3/3 · ✓ цитаты 4/4 дословно (3 — из другого фрагмента) · ✓ числа 6/6 в цитатах · ✗ смысл по цитатам: частично
#### 1.05 Зачем таймауты выборов рандомизированы?

_контекст:_
1. #1 cos 0.9961 z 4.15 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.
2. #8 cos 0.9925 z 3.12 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 6 · 998 симв.
3. #2 cos 0.9935 z 3.42 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 10–11 · 1000 симв.
4. #12 cos 0.9917 z 2.90 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 15 · 994 симв.

> Рандомизация таймаутов выборов нужна, чтобы split votes (разделение голосов) были редкими и быстро разрешались: таймауты выбираются случайно из фиксированного интервала (например, 150–300 мс), из-за чего обычно только один сервер успевает тайм-аут, выигрывает выборы и рассылает сердцебиения раньше остальных. Кроме того, каждый кандидат перезапускает свой случайный таймаут перед новой попыткой, что снижает вероятность повторного split vote. [1]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 6 · fixed-raft-consensus-0034
> 
> Цитаты:
>   [1] «Raft uses randomized election timeouts to ensure that split votes are rare and that they are resolved quickly.»
>   [1] «election timeouts are chosen randomly from a fixed interval (e.g., 150–300ms). This spreads out the servers so that in most cases only a single server will time out; it wins the election and sends heartbeats before any other servers time out.»
>   [1] «Each candidate restarts its randomized election timeout at the start of an election, and it waits for that timeout to elapse before starting the next election; this reduces the likelihood of another split vote in the new election.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✓ числа 2/2 в цитатах · ✓ смысл по цитатам: да

#### 1.06 Согласуем термин: для нас «quorum = 3». Как лидер надёжно фиксирует запись в логе?

_контекст:_
1. #3 cos 0.9957 z 3.21 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 7 · 994 симв.
2. #17 cos 0.9943 z 2.80 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 6–7 · 997 симв.
3. #2 cos 0.9957 z 3.21 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 9 · 1000 симв.
4. #4 cos 0.9953 z 3.11 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 8 · 1000 симв.

> Лидер считает запись committed, когда реплицировал её на большинство серверов; committed-записи гарантированно durable и будут выполнены всеми доступными state machines. При этом все предыдущие записи в логе лидера также считаются committed, лидер хранит высший известный committed-индекс и передаёт его в AppendEntries RPC (включая heartbeats), а фолловер, узнав о committed-записи, применяет её к своей state machine в порядке лога. [1]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 7 · fixed-raft-consensus-0038
> 
> Цитаты:
>   [1] «A log entry is committed once the leader that created the entry has replicated it on a majority of the servers»
>   [1] «This also commits all preceding entries in the leader’s log»
>   [1] «Once a follower learns that a log entry is committed, it applies the entry to its local state machine (in log order).»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично
#### 1.07 Ещё требование: чтения должны быть строго актуальными, никаких устаревших значений. Что это меняет для нас по статье?

_контекст:_
1. #10 cos 0.9887 z 2.65 llm 9 (rewrite) · raft-consensus.pdf — Front matter, стр. 13 · 996 симв.
2. #7 cos 0.9890 z 2.76 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 13 · 998 симв.
3. #2 cos 0.9893 z 2.90 llm 5 · raft-consensus.pdf — Front matter, стр. 8 · 995 симв.

> Чтения можно обрабатывать без записи в лог, но без дополнительных мер есть риск возврата устаревших данных, если лидера уже сместил более новый лидер. Для линеаризуемых чтений нужны две меры: лидер должен знать актуальный commit-статус — для этого он в начале срока коммитит пустую no-op запись; и перед ответом на read-only запрос лидер обязан убедиться, что его не сместили, обменявшись heartbeat-сообщениями с большинством кластера. Альтернатива — lease через heartbeat, но она опирается на тайминги и допускает ограниченный skew часов. [1][2]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 13 · fixed-raft-consensus-0081
>   [2] raft-consensus.pdf — Front matter, стр. 13 · fixed-raft-consensus-0080
> 
> Цитаты:
>   [2] «Read-only operations can be handled without writing anything into the log. However, with no additional measures, this would run the risk of returning stale data, since the leader responding to the request might have been superseded by a newer leader of which it is unaware.»
>   [1] «a leader must check whether it has been deposed before processing a read-only request»
>   [1] «Raft handles this by having the leader exchange heartbeat messages with a majority of the cluster before responding to read-only requests. Alternatively, the leader could rely on the heartbeat mechanism to provide a form of lease [9], but this would rely on timing for safety (it assumes bounded clock skew).»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично
#### 1.08 Состав кластера менять придётся, но без даунтайма. Как Raft меняет конфигурацию?

_контекст:_
1. #20 cos 0.9927 z 2.65 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 11 · 991 симв.
2. #1 cos 0.9952 z 3.38 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 10 · 997 симв.
3. #3 cos 0.9951 z 3.37 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 1 · 1000 симв.
4. #10 cos 0.9934 z 2.87 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 10 · 992 симв.

> Прямое переключение всех серверов со старой конфигурации на новую небезопасно — кластер может расколоться на два независимых большинства, поэтому изменения должны быть двухфазными. Raft использует joint consensus: сначала кластер переходит в переходную конфигурацию, объединяющую старую и новую, а после её коммита — в новую конфигурацию. Записи лога реплицируются на все серверы обеих конфигураций, а согласие требует отдельных большинства от старой и новой конфигураций. Это позволяет кластеру продолжать нормально работать во время изменения конфигурации. [4][2][1]
> 
> Источники:
>   [4] raft-consensus.pdf — Front matter, стр. 10 · fixed-raft-consensus-0061
>   [2] raft-consensus.pdf — Front matter, стр. 10 · fixed-raft-consensus-0062
>   [1] raft-consensus.pdf — Front matter, стр. 11 · fixed-raft-consensus-0067
> 
> Цитаты:
>   [4] «any approach where servers switch directly from the old configuration to the new configuration is unsafe.»
>   [2] «In Raft the cluster first switches to a transitional configuration we call joint consensus; once the joint consensus has been committed, the system then transitions to the new configuration.»
>   [1] «Agreement (for elections and entry commitment) requires separate majorities from both the old and new configurations.»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 3/3 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично
#### 1.09 А новый узел, который только поднялся, — он сразу голосует?

_контекст:_
1. #18 cos 0.9912 z 2.77 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 10–11 · 993 симв.
2. #5 cos 0.9924 z 3.07 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 5–6 · 999 симв.

> Нет, сразу участвовать в кворуме он не может. Чтобы избежать провалов в доступности, Raft вводит дополнительную фазу: новый сервер присоединяется как non-voting member — лидер реплицирует ему записи лога, но при расчёте большинства он не учитывается. Только после того, как новый сервер догонит остальной кластер, изменение конфигурации продолжается как обычно. При старте любой сервер, кстати, начинает как фолловер. [1][2]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 10–11 · fixed-raft-consensus-0064
>   [2] raft-consensus.pdf — Front matter, стр. 5–6 · fixed-raft-consensus-0031
> 
> Цитаты:
>   [1] «Raft introduces an additional phase before the configuration change, in which the new servers join the cluster as non-voting members (the leader replicates log entries to them, but they are not considered for majorities).»
>   [1] «Once the new servers have caught up with the rest of the cluster, the reconfiguration can proceed as described above.»
>   [2] «When servers start up, they begin as followers.»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 1.10 Если узел отстал на очень много записей, как его наверстать?

_контекст:_
1. #11 cos 0.9917 z 2.79 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 12 · 992 симв.
2. #9 cos 0.9918 z 2.80 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 7 · 998 симв.
3. #7 cos 0.9920 z 2.86 llm 6 (rewrite) · raft-consensus.pdf — Front matter, стр. 8 · 1000 симв.

> Если фолловер отстал настолько, что лидер уже отбросил нужные ему записи лога, лидер отправляет ему снапшот по сети через новый RPC InstallSnapshot: он передаёт состояние на момент last included index вместо всех записей [1]. Это нетипично для нормальной работы — отставший фолловер обычно успевает получить записи через AppendEntries — но касается исключительно медленных фолловеров и новых серверов [1].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 12 · fixed-raft-consensus-0075
> 
> Цитаты:
>   [1] «The leader uses a new RPC called InstallSnapshot to send snapshots to followers that are too far behind»
>   [1] «This happens when the leader has already discarded the next log entry that it needs to send to a follower.»
>   [1] «an exceptionally slow follower or a new server joining the cluster (Section 6) would not.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 1.11 Напомни, сколько у нас узлов в кластере и какой кворум?

_контекст:_
1. #1 cos 0.9920 z 3.62 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 5 · 998 симв.
2. #12 cos 0.9902 z 3.05 llm 5 (rewrite) · raft-consensus.pdf — Front matter, стр. 15–16 · 993 симв.

> В кластере 5 узлов — типичное число, которое допускает отказ двух серверов [1]. Кворум — большинство узлов кластера (как мы зафиксировали: 3 из 5). [1]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 5 · fixed-raft-consensus-0029
> 
> Цитаты:
>   [1] «A Raft cluster contains several servers; five is a typical number, which allows the system to tolerate two failures.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✗ числа 0/2 в цитатах · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✗ числа 0/2 в цитатах · ✗ смысл по цитатам: частично
#### 1.12 Что статья о Raft говорит про шардирование PostgreSQL?

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — llm 0))
> Уточните, пожалуйста: о каком документе или системе вопрос «Что статья о Raft говорит про шардирование PostgreSQL?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: raft-consensus.pdf — Front matter, стр. 3 (llm 0)
>   ближе всего: raft-consensus.pdf — Front matter, стр. 1 (llm 0)
>   ближе всего: raft-consensus.pdf — Front matter, стр. 15 (llm 0)

#### 1.13 Как называется переходная конфигурация при смене состава кластера?

_контекст:_
1. #1 cos 0.9927 z 4.03 llm 10 (rewrite) · raft-consensus.pdf — Front matter, стр. 10 · 997 симв.
2. #4 cos 0.9917 z 3.75 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 11 · 991 симв.
3. #6 cos 0.9905 z 3.38 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 1 · 1000 симв.
4. #14 cos 0.9885 z 2.81 llm 8 (rewrite) · raft-consensus.pdf — Front matter, стр. 11 · 991 симв.

> Переходная конфигурация при смене состава кластера называется joint consensus [1]. [1]
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 10 · fixed-raft-consensus-0062
> 
> Цитаты:
>   [1] «In Raft the cluster first switches to a transitional configuration we call joint consensus; once the joint consensus has been committed, the system then transitions to the new configuration.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 1/1 дословно · ✓ смысл по цитатам: да

#### 1.14 Собери итог списком: цель проекта и все зафиксированные ограничения и термины.

_контекст:_
1. #2 cos 0.9924 z 3.09 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 10 · 997 симв.
2. #4 cos 0.9919 z 2.93 llm 7 (rewrite) · raft-consensus.pdf — Front matter, стр. 10 · 1000 симв.
3. #8 cos 0.9916 z 2.83 llm 6 (rewrite) · raft-consensus.pdf — Front matter, стр. 1 · 1000 симв.
4. #9 cos 0.9877 z 2.74 llm 6 · raft-consensus.pdf — Front matter, стр. 5 · 998 симв.

> Цель проекта: спроектировать внутренний распределённый сервис конфигураций на основе Raft, устойчивый к отказам узлов.
> 
> Зафиксированные ограничения:
> - Кластер из 5 узлов, кворум = 3.
> - Строго актуальные (линеаризуемые) чтения.
> - Смена состава кластера без даунтайма.
> 
> Согласованные термины:
> - Raft — алгоритм консенсуса для реплицируемого лога: выборы лидера, репликация лога, безопасность.
> - Кворум — большинство узлов кластера; в этом диалоге зафиксировано quorum = 3.
> - Election timeout — случайный таймаут: фолловер без связи с лидером становится кандидатом.
> - RequestVote RPC — запрос голоса, который кандидат рассылает другим серверам на выборах [4].
> - Broadcast time — время рассылки RPC, должно быть на порядок меньше election timeout [2].
> - MTBF — среднее время между отказами; election timeout должен быть на порядки меньше MTBF [2].
> - Split vote — разделение голосов; разрешается благодаря рандомизации таймаутов выборов.
> - Committed запись — реплицирована на большинство; durable и будет применена всеми state machines.
> - Commit index — высший известный committed-индекс; передаётся в AppendEntries, фолловеры применяют записи по порядку.
> - Joint consensus — двухфазное изменение конфигурации: переходная конфигурация (старая+новая, отдельные большинства), затем новая; без даунтайма [1][3].
> 
> Источники:
>   [1] raft-consensus.pdf — Front matter, стр. 10 · fixed-raft-consensus-0062
>   [2] raft-consensus.pdf — Front matter, стр. 10 · fixed-raft-consensus-0060
>   [3] raft-consensus.pdf — Front matter, стр. 1 · fixed-raft-consensus-0001
>   [4] raft-consensus.pdf — Front matter, стр. 5 · fixed-raft-consensus-0029
> 
> Цитаты:
>   [1] «first switches to a transitional configuration we call joint consensus; once the joint consensus has been committed, the system then transitions to the new configuration.»
>   [2] «The broadcast time should be an order of magnitude less than the election timeout so that leaders can reliably send the heartbeat messages»
>   [3] «Raft’s mechanism for changing the set of servers in the cluster uses a new joint consensus approach where the majorities of two different configurations overlap during transitions.»
>   [4] «RequestVote RPCs are initiated by candidates during elections»
> 
> Проверка: ✓ источники 4/4 · ✓ цитаты 4/4 дословно (1 — из другого фрагмента) · ✗ числа 0/2 в цитатах · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 4/4 · ✓ цитаты 4/4 дословно (1 — из другого фрагмента) · ✗ числа 0/2 в цитатах · ✗ смысл по цитатам: частично
## Сценарий 2: Production-чат по документации компании (rag-survey-2312.10997.pdf)

Ожидаемая цель: собрать по обзору RAG production-чат, отвечающий по документации компании с обязательными источниками: 10k чанков, обязательный реранкер

Цель в памяти после диалога: «Собрать production-чат, отвечающий по внутренней документации компании, используя RAG, и понять парадигмы RAG из обзора.» (2/2)

Память в конце:

```json
{
  "goal": "Собрать production-чат, отвечающий по внутренней документации компании, используя RAG, и понять парадигмы RAG из обзора.",
  "clarified": [
    "Чат работает по внутренней документации компании",
    "Цель — production-внедрение",
    "RAG сравнивается с fine-tuning: RAG лучше для точного извлечения и свежих знаний",
    "Оптимизацию индексации берём из обзора (иерархия, метаданные, время)",
    "Интересуют эмбеддинг-модели, рассматриваемые обзором",
    "Реранкер в пайплайне — обязательное решение",
    "Интересует улучшение запроса до поиска (pre-retrieval)",
    "Качество чата оценивается метриками retrieval и generation"
  ],
  "constraints": [
    "Каждый ответ должен содержать источники",
    "Реранкер обязателен в пайплайне"
  ],
  "terms": [
    {
      "term": "Naive RAG",
      "meaning": "Базовая парадигма RAG, имеет ограничения"
    },
    {
      "term": "Advanced RAG",
      "meaning": "Парадигма, развившаяся как ответ на ограничения Naive RAG"
    },
    {
      "term": "Modular RAG",
      "meaning": "Парадигма с модульной архитектурой, эволюция Advanced RAG"
    },
    {
      "term": "Indexing",
      "meaning": "Этап RAG-пайплайна: чанкинг документов, векторизация, сохранение в векторную БД"
    },
    {
      "term": "Retrieval",
      "meaning": "Этап извлечения Top-k чанков, релевантных вопросу по семантической близости"
    },
    {
      "term": "Generation",
      "meaning": "Этап передачи вопроса и извлечённых чанков в LLM для финального ответа"
    },
    {
      "term": "Fine-tuning",
      "meaning": "Дообучение модели: нужно для кастомизации поведения и стиля, не взаимно исключает RAG"
    },
    {
      "term": "Родитель-потомок чанкинг",
      "meaning": "Иерархическая структура индекса: мелкие чанки для поиска, крупные — для контекста"
    },
    {
      "term": "Reverse HyDE",
      "meaning": "Метаданные индекса: краткие содержания абзацев и гипотетические вопросы для улучшения поиска"
    },
    {
      "term": "Retrieval/Generation Quality",
      "meaning": "Две цели оценки качества RAG: retrieval — Hit Rate, MRR, NDCG; generation — faithfulness, relevance, non-harmfulness, EM/F1, Accuracy, BLEU/ROUGE"
    }
  ]
}
```

| # | вопрос | статус | источники | ожидание | память |
|---|---|---|---|---|---|
| 1 | Собираем production-чат, который отвечает по внутренней документации компании, через RAG. Какие парадигмы RAG выделяет обзор? | answer (ждали answer) | 2/2 | 3/3 | ✓ |
| 2 | Уточнение: объём небольшой, порядка 10k чанков. С какой парадигмы разумно начать? | unknown (ждали answer) | 0/0 | 0/1 | нет: 10k|10 k|10 000|10 тыс |
| 3 | Фиксируем требование: каждый ответ — с источниками. Из каких трёх этапов состоит классический RAG-пайплайн? | answer (ждали answer) | 2/2 | 3/3 | ✓ |
| 4 | Чем RAG лучше дообучения модели, и когда всё-таки нужен fine-tuning? | answer (ждали answer) | 3/3 | 1/2 | — |
| 5 | Чанкование: как оптимизировать индекс по обзору? | answer (ждали answer) | 1/1 | 0/2 | — |
| 6 | Какие эмбеддинг-модели рассматривает обзор? | answer (ждали answer) | 1/1 | 1/1 | — |
| 7 | Решение зафиксировано: реранкер обязателен. Что обзор говорит про reranking? | answer (ждали answer) | 1/1 | 1/1 | ✓ |
| 8 | Как можно улучшить сам запрос до поиска? | answer (ждали answer) | 1/1 | 0/1 | — |
| 9 | Что обзор говорит про цены на GPU у AWS в 2026 году? | unknown (ждали unknown) | 0/0 | — | — |
| 10 | Метрики: по каким оценивать качество нашего чата? | answer (ждали answer) | 2/2 | 2/2 | — |
| 11 | На каких фреймворках можно собрать прототип? | unknown (ждали answer) | 0/0 | 0/1 | — |
| 12 | Напомни, какой реранкер мы зафиксировали и каков объём базы? | answer (ждали answer) | 0/0 | 2/2 | — |
| 13 | Дай финальный план в 5 шагов: индекс, поиск, реранк, генерация, источники — с учётом всего, что записано. | answer (ждали answer) | 1/1 | 1/2 | нет: 10k|10 000 |

### Ответы

#### 2.01 Собираем production-чат, который отвечает по внутренней документации компании, через RAG. Какие парадигмы RAG выделяет обзор?

_контекст:_
1. #3 cos 0.9921 z 3.40 llm 8 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 · 995 симв.
2. #4 cos 0.9918 z 3.29 llm 8 (rewrite) · rag-survey-2312.10997.pdf — I. INTRODUCTION, стр. 2 · 994 симв.
3. #5 cos 0.9918 z 3.28 llm 8 (rewrite) · rag-survey-2312.10997.pdf — I. INTRODUCTION, стр. 1–2 · 999 симв.
4. #6 cos 0.9917 z 3.24 llm 8 (rewrite) · rag-survey-2312.10997.pdf — Front matter, стр. 1 · 998 симв.

> Обзор выделяет три парадигмы RAG: Naive RAG, Advanced RAG и Modular RAG [1][4]. Развитие Advanced RAG и Modular RAG стало ответом на ограничения Naive RAG [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 · fixed-rag-survey-2312.10997-0010
>   [4] rag-survey-2312.10997.pdf — Front matter, стр. 1 · fixed-rag-survey-2312.10997-0001
> 
> Цитаты:
>   [1] «we categorize it into three stages: Naive RAG, Advanced
> RAG, and Modular RAG»
>   [1] «The development of Advanced RAG and Modular RAG is
> a response to these specific shortcomings in Naive RAG.»
>   [4] «encompassing
> the Naive RAG, the Advanced RAG, and the Modular RAG.»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 2.02 Уточнение: объём небольшой, порядка 10k чанков. С какой парадигмы разумно начать?

_контекст:_
1. #13 cos 0.9888 z 3.09 llm 5 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · 998 симв.

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (лучший фрагмент — llm 5 < порога 7)
> Уточните, пожалуйста: о каком документе или системе вопрос «Уточнение: объём небольшой, порядка 10k чанков. С какой парадигмы разумно начать?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 (llm 5)
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 (llm 4)
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 (llm 4)

#### 2.03 Фиксируем требование: каждый ответ — с источниками. Из каких трёх этапов состоит классический RAG-пайплайн?

_контекст:_
1. #1 cos 0.9959 z 4.44 llm 10 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 · 995 симв.
2. #6 cos 0.9928 z 3.35 llm 10 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · 998 симв.
3. #2 cos 0.9948 z 4.07 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 · 1000 симв.

> Классический (Naive) RAG-пайплайн состоит из трёх этапов: 1) индексация (Indexing) — документы разбиваются на чанки, кодируются в векторы и сохраняются в векторную БД; 2) извлечение (Retrieval) — Top-k чанков, наиболее релевантных вопросу по семантической близости; 3) генерация (Generation) — исходный вопрос и извлечённые чанки передаются в LLM для финального ответа [1][2].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 · fixed-rag-survey-2312.10997-0010
>   [2] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · fixed-rag-survey-2312.10997-0011
> 
> Цитаты:
>   [1] «1) Indexing. Documents are split into chunks, encoded into vectors, and stored in a vector database. 2) Retrieval. Retrieve the Top k chunks most relevant to the question based on semantic similarity. 3) Generation. Input the original question and the retrieved chunks together into LLM to generate the final answer.»
>   [2] «The Naive RAG follows a traditional process that includes indexing, retrieval, and generation»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 2/2 дословно · ✓ числа 3/3 в цитатах · ✓ смысл по цитатам: да

#### 2.04 Чем RAG лучше дообучения модели, и когда всё-таки нужен fine-tuning?

_контекст:_
1. #9 cos 0.9927 z 2.81 llm 10 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · 995 симв.
2. #11 cos 0.9911 z 2.77 llm 9 · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · 994 симв.
3. #13 cos 0.9923 z 2.70 llm 9 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · 998 симв.
4. #4 cos 0.9932 z 2.98 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > C. Modular RAG, стр. 5 · 992 симв.

> RAG лучше подходит для задач точного извлечения информации: он обеспечивает обновление знаний в реальном времени, эффективное использование внешних источников с высокой интерпретируемостью и стабильно превосходит несупервизированное дообучение на знаниеёмких задачах — как по знаниям из обучения, так и по совершенно новым [1][2]. Fine-tuning нужен, когда требуется глубокая кастомизация поведения и стиля модели — например, воспроизведение специфических структур, стилей или форматов; выбор зависит от динамики данных, потребности в кастомизации и вычислительных ресурсов, а RAG и FT не взаимоисключающи и могут дополнять друг друга, вплоть до совместного использования для оптимального результата [1][2][3].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · fixed-rag-survey-2312.10997-0025
>   [2] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · fixed-rag-survey-2312.10997-0024
>   [3] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > D. RAG vs Fine-tuning, стр. 5 · fixed-rag-survey-2312.10997-0026
> 
> Цитаты:
>   [2] «RAG can be likened to providing a model with a tailored textbook for information retrieval, ideal for precise information retrieval tasks. In contrast, FT is comparable to a student internalizing knowledge over time, suitable for scenarios requiring replication of specific structures, styles, or formats.»
>   [1] «RAG excels in dynamic environments by offering realtime knowledge updates and effective utilization of external knowledge sources with high interpretability.»
>   [1] «while unsupervised fine-tuning shows some improvement, RAG consistently outperforms it, for both existing knowledge encountered during training and entirely new knowledge.»
> 
> Проверка: ✓ источники 3/3 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 2.05 Чанкование: как оптимизировать индекс по обзору?

_контекст:_
1. #3 cos 0.9931 z 2.65 llm 7 (rewrite) · rag-survey-2312.10997.pdf — III. RETRIEVAL > B. Indexing Optimization, стр. 8 · 1000 симв.
2. #4 cos 0.9931 z 2.63 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 · 1000 симв.
3. #5 cos 0.9930 z 2.61 llm 7 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 4 · 997 симв.
4. #2 cos 0.9933 z 2.71 llm 5 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · 998 симв.

> По обзору оптимизация индексации включает: установку иерархической структуры «родитель-потомок», добавление искусственно созданных метаданных — кратких содержаний абзацев и гипотетических вопросов (Reverse HyDE), а также взвешивание меток времени документов для time-aware RAG, обеспечивающего свежесть знаний [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — III. RETRIEVAL > B. Indexing Optimization, стр. 8 · fixed-rag-survey-2312.10997-0041
> 
> Цитаты:
>   [1] «Hierarchical index structure. File are arranged in parentchild»
>   [1] «metadata can also be artificially constructed. For example, adding summaries of paragraph, as well as introducing hypothetical questions. This method is also known as Reverse HyDE.»
>   [1] «Assigning different weights to document timestamps during retrieval can achieve time-aware RAG, ensuring the freshness of knowledge and avoiding outdated information.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 2.06 Какие эмбеддинг-модели рассматривает обзор?

_контекст:_
1. #2 cos 0.9909 z 3.43 llm 10 (rewrite) · rag-survey-2312.10997.pdf — III. RETRIEVAL > C. Query Optimization, стр. 9 · 1000 симв.

> Обзор выделяет два типа эмбеддинг-моделей: разреженный энкодер (BM25) и плотный ретривер на базе BERT-архитектуры; также упоминаются современные модели AngIE, Voyage, BGE, которые выигрывают от multi-task instruct tuning. Рекомендаций «какая модель лучше» нет — выбор зависит от конкретного сценария [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — III. RETRIEVAL > C. Query Optimization, стр. 9 · fixed-rag-survey-2312.10997-0051
> 
> Цитаты:
>   [1] «This mainly includes a sparse encoder (BM25) and a dense retriever (BERT architecture Pre-training language models).»
>   [1] «Recent research has introduced prominent embedding models such as AngIE, Voyage, BGE,etc [94]–[96], which are benefit from multi-task instruct tuning.»
>   [1] «There is no one-size-fits-all answer to “which embedding model to use.”»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 2.07 Решение зафиксировано: реранкер обязателен. Что обзор говорит про reranking?

_контекст:_
1. #1 cos 0.9934 z 3.12 llm 10 (rewrite) · rag-survey-2312.10997.pdf — IV. GENERATION > A. Context Curation, стр. 10 · 999 симв.

> Обзор описывает реранкинг так: наиболее подходящие результаты ставятся первыми, что сокращает общий пул документов и служит двойной цели — усилителя и фильтра, давая LLM более точные входы [1]. Реранкинг может выполняться rule-based методами (метрики Diversity, Relevance, MRR) или model-based: Encoder-Decoder модели серии BERT (например, SpanBERT), специализированные реранкеры вроде Cohere rerank или bge-raranker-large, а также общие LLM типа GPT [1]. [1]
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — IV. GENERATION > A. Context Curation, стр. 10 · fixed-rag-survey-2312.10997-0056
> 
> Цитаты:
>   [1] «most pertinent results first, effectively reducing the overall document pool, severing a dual purpose in information retrieval, acting as both an enhancer and a filter»
>   [1] «delivering refined inputs for more precise language model processing [70]»
>   [1] «Reranking can be performed using rule-based methods that depend on predefined metrics like Diversity, Relevance, and MRR, or model-based approaches like Encoder-Decoder models from the BERT series (e.g., SpanBERT), specialized reranking models such as Cohere rerank or bge-raranker-large, and general large language models like GPT»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✓ смысл по цитатам: да

#### 2.08 Как можно улучшить сам запрос до поиска?

_контекст:_
1. #9 cos 0.9894 z 2.53 llm 8 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 4 · 997 симв.
2. #7 cos 0.9895 z 2.59 llm 7 (rewrite) · rag-survey-2312.10997.pdf — III. RETRIEVAL > C. Query Optimization, стр. 9 · 1000 симв.
3. #8 cos 0.9895 z 2.57 llm 6 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > C. Modular RAG, стр. 5 · 998 симв.
4. #11 cos 0.9893 z 2.51 llm 6 (rewrite) · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > C. Modular RAG, стр. 5 · 995 симв.

> До поиска запрос можно улучшить на этапе pre-retrieval query optimization: сделать исходный вопрос пользователя более ясным и подходящим для retrieval-задачи. Обзор называет методы: query rewriting, query transformation, query expansion и другие техники [1].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 4 · fixed-rag-survey-2312.10997-0016
> 
> Цитаты:
>   [1] «While the goal of query optimization is to make the user’s original question clearer and more suitable for the retrieval task. Common methods include query rewriting query transformation, query expansion and other techniques»
>   [1] «the primary focus is on optimizing the indexing structure and the original query.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 2/2 дословно · ✓ смысл по цитатам: да

#### 2.09 Что обзор говорит про цены на GPU у AWS в 2026 году?

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — llm 0))
> Уточните, пожалуйста: о каком документе или системе вопрос «Что обзор говорит про цены на GPU у AWS в 2026 году?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 3–4 (llm 0)
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG, стр. 2–3 (llm 0)
>   ближе всего: rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > B. Advanced RAG, стр. 4 (llm 0)

#### 2.10 Метрики: по каким оценивать качество нашего чата?

_контекст:_
1. #1 cos 0.9942 z 3.08 llm 10 (rewrite) · rag-survey-2312.10997.pdf — VI. TASK AND EVALUATION > B. Evaluation Target, стр. 12 · 993 симв.
2. #5 cos 0.9930 z 2.65 llm 9 (rewrite) · rag-survey-2312.10997.pdf — VII. DISCUSSION AND FUTURE PROSPECTS > B. RAG Robustness, стр. 14–15 · 992 симв.
3. #9 cos 0.9927 z 2.53 llm 9 (rewrite) · rag-survey-2312.10997.pdf — VII. DISCUSSION AND FUTURE PROSPECTS > C. Hybrid Approaches, стр. 15 · 998 симв.
4. #7 cos 0.9929 z 2.60 llm 8 (rewrite) · rag-survey-2312.10997.pdf — VI. TASK AND EVALUATION > A. Downstream Task, стр. 12 · 998 симв.

> Качество оценивается по двум целям: Retrieval Quality и Generation Quality [1][4]. Для retrieval используются стандартные метрики поисковых/рекомендательных систем: Hit Rate, MRR, NDCG [1]. Для генерации — на безразмеченном контенте faithfulness, relevance и non-harmfulness; на размеченном — точность информации [1]. Для задач QA — EM и F1, фактчекинг — Accuracy, качество текста — BLEU/ROUGE [4].
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — VI. TASK AND EVALUATION > B. Evaluation Target, стр. 12 · fixed-rag-survey-2312.10997-0070
>   [4] rag-survey-2312.10997.pdf — VI. TASK AND EVALUATION > A. Downstream Task, стр. 12 · fixed-rag-survey-2312.10997-0069
> 
> Цитаты:
>   [1] «Metrics such as Hit Rate, MRR, and NDCG are commonly utilized for this purpose»
>   [1] «the evaluation encompasses the faithfulness, relevance, and non-harmfulness of the generated answers. In contrast, for labeled content, the focus is on the accuracy of the information produced by the model»
>   [4] «question answering evaluations might rely on EM and F1 scores [7], [45], [59], [72], whereas fact-checking tasks often hinge on Accuracy as the primary metric [4], [14], [42]. BLEU and ROUGE metrics are also commonly used to evaluate answer quality [26], [32], [52], [78].»
> 
> Проверка: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 2/2 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично
#### 2.11 На каких фреймворках можно собрать прототип?

> Не знаю: в найденных фрагментах нет ответа на этот вопрос. (ни один фрагмент не прошёл фильтр (лучший кандидат — llm 0))
> Уточните, пожалуйста: о каком документе или системе вопрос «На каких фреймворках можно собрать прототип?», и что именно нужно узнать (термин, число, раздел)? Например, речь о чём-то из этого?
>   ближе всего: rag-survey-2312.10997.pdf — Front matter, стр. 1 (llm 0)
>   ближе всего: rag-survey-2312.10997.pdf — III. RETRIEVAL > B. Indexing Optimization, стр. 8 (llm 0)
>   ближе всего: rag-survey-2312.10997.pdf — III. RETRIEVAL > A. Retrieval Source, стр. 7 (llm 0)

#### 2.12 Напомни, какой реранкер мы зафиксировали и каков объём базы?

> Из истории диалога:
> 
> - **Реранкер:** мы зафиксировали требование, что реранкер **обязателен** в пайплайне, но конкретную модель не выбирали. Обзор упоминал варианты: rule-based методы, модели серии BERT (SpanBERT), специализированные реранкеры (Cohere rerank, bge-raranker-large) и общие LLM (GPT).
> - **Объём базы:** порядка **10k чанков** (вы уточняли это в контексте выбора парадигмы RAG для старта).
> 
> Из памяти задачи: ответ дан из цели и договорённостей этого диалога, а не из фрагментов корпуса — источников нет.

- ✗ проверки: 
#### 2.13 Дай финальный план в 5 шагов: индекс, поиск, реранк, генерация, источники — с учётом всего, что записано.

_контекст:_
1. #3 cos 0.9897 z 2.53 llm 8 · rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · 998 симв.
2. #2 cos 0.9899 z 2.62 llm 5 · rag-survey-2312.10997.pdf — III. RETRIEVAL > A. Retrieval Source, стр. 7 · 993 симв.

> Финальный план production-чата по внутренней документации: Шаг «Индекс» — очистить и извлечь сырые данные в форматах PDF, HTML, Word, Markdown, привести к единому тексту, сегментировать на чанки с иерархией «родитель-потомок» и метаданными (Reverse HyDE, time-aware веса), закодировать эмбеддинг-моделью и сохранить в векторную БД. Шаг «Поиск» — тем же энкодером кодировать запрос пользователя с pre-retrieval оптимизацией (rewriting, transformation, expansion) и извлечь Top-k чанков по семантической близости. Шаг «Реранк» — обязательный реранкинг для сокращения пула документов перед генерацией. Шаг «Генерация» — передать исходный вопрос и извлечённые чанки в LLM для финального ответа. Шаг «Источники» — каждый ответ сопровождать источниками и оценивать качество по Retrieval Quality (Hit Rate, MRR, NDCG) и Generation Quality (faithfulness, relevance, non-harmfulness, EM/F1, Accuracy, BLEU/ROUGE).
> 
> Источники:
>   [1] rag-survey-2312.10997.pdf — II. OVERVIEW OF RAG > A. Naive RAG, стр. 3 · fixed-rag-survey-2312.10997-0011
> 
> Цитаты:
>   [1] «Indexing starts with the cleaning and extraction of raw data in diverse formats like PDF, HTML, Word, and Markdown, which is then converted into a uniform plain text format.»
>   [1] «Chunks are then encoded into vector representations using an embedding model and stored in vector database.»
>   [1] «3) Generation. Input the original question and the retrieved chunks together into LLM to generate the final answer.»
> 
> Проверка: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично

- ✗ проверки: ✓ источники 1/1 · ✓ цитаты 3/3 дословно · ✗ смысл по цитатам: частично