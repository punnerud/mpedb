# PostgreSQL LISTEN/NOTIFY, fra kilden

Lest i `src/backend/commands/async.c` på `master` (3299 linjer, hentet
2026-07-25). Linjenumre gjelder den revisjonen. Dette dokumentet finnes fordi
[varslings-benchmarken](../benchmarks/notify.md) sammenligner mot Postgres og bør
kunne forsvare påstandene sine på kildenivå, ikke bare mot en blogg.

## Hva som serialiserer

Ikke kø-låsen. En **tungvektslås på «database 0»** (async.c:1309):

```c
LockSharedObject(DatabaseRelationId, InvalidOid, 0, AccessExclusiveLock);
```

`InvalidOid` som database-oid gjør den **klynge-global**, ikke per database.
Kommentaren rett over sier hvorfor, ordrett (async.c:1293–1301):

> Serialize writers by acquiring a special lock that we hold till after commit.
> This ensures that queue entries appear in commit order, and in particular that
> there are never uncommitted queue entries ahead of committed ones, so an
> uncommitted transaction can't block delivery of deliverable notifications.
>
> We use a heavyweight lock so that it'll automatically be released after either
> commit or abort. […] The lock is on "database 0", which is pretty ugly but it
> doesn't seem worth inventing a special locktag category just for this.

## Hvor lenge den holdes

Tungvektslåser slippes ved transaksjonsslutt, altså etter at commit-posten er
flushet. For en varslende transaksjon spenner låsen derfor over tre ting:

| Steg | Hva skjer | async.c |
|---|---|---|
| 1 | `PreCommit_Notify()` tar låsen, legger postene i SLRU-køen | 1309, `asyncQueueAddEntries` |
| 2 | `RecordTransactionCommit()` — WAL-skriving og **fsync** | (xact.c) |
| 3 | `AtCommit_Notify()` → `SignalBackends()` | 1403, 2263 |
| 4 | Transaksjonsslutt — låsen slippes | |

Steg 2 er den dyre. At den ligger *inne* i låsen er hele DBOS-funnet, og kilden
bekrefter det: «we hold till after commit».

## Signaleringen: to nivåer, og bare det ene filtrerer

`SignalBackends()` (async.c:2263) kjører **under `LWLockAcquire(NotifyQueueLock,
LW_EXCLUSIVE)`** og gjør to gjennomløp:

**Første gjennomløp — kanal-indeksert.** Moderne PG har `globalChannelTable`, en
dshash på `(MyDatabaseId, channel)` med en `listenersArray` per kanal. For hver
kanal transaksjonen varslet slås lytterne opp direkte. Det er ekte filtrering, og
det er verdt å si tydelig: påstanden «Postgres kan ikke vite hvem som bryr seg»
er **utdatert**.

**Andre gjennomløp — alle lyttere.** Deretter (async.c:2337):

```c
for (ProcNumber i = QUEUE_FIRST_LISTENER; i != INVALID_PROC_NUMBER; i = QUEUE_NEXT_LISTENER(i))
```

Hele lytterlista i klyngen, for direct-advance-optimaliseringen — å flytte en
uinteressert lytters køpeker fram i stedet for å vekke den. Nyttig i seg selv,
men det gjør kostnaden per varsel **O(alle lyttere)**, ikke O(interesserte), og
den betales under den eksklusive `NotifyQueueLock`.

## Det som er verdt å hente ut

**Filtreringsgranularitet og serialiseringsgranularitet er frakoblet.** Postgres
kan filtrere per kanal, men serialiserer per klynge. Du kan ha 10 000 kanaler og
fortsatt kjøre hver varslende commit gjennom én eksklusiv lås. Det er ikke en
mangel på filtrering — det er at filteret ikke er koblet til det som koster.

Grunnen er strukturell: køen er **én delt, ordnet struktur**, og
ordningsgarantien («aldri ucommittede poster foran committede») er det som
krever serialisering. Ikke leveransen — ordningen.

**Kilden flagger den selv** (async.c:1316):

> Note: if the heavyweight lock were ever removed for scalability reasons, we
> could achieve the same guarantee by holding NotifyQueueLock in EXCLUSIVE mode
> across all our insertions, rather than releasing and reacquiring it for each
> page as we do below.

## Øvrige rammer

- **Kø:** SLRU, `max_notify_queue_pages = 1048576` → 8 GB ved 8 KB sider (:584).
- **Payload:** `BLCKSZ - NAMEDATALEN - 128` ≈ 7,8 KB (:201) — én SLRU-side.
- **Halen** flyttes til minimum av alle lytteres posisjon (`asyncQueueAdvanceTail`,
  :2870), forsøkt hver `QUEUE_CLEANUP_DELAY = 4` sider (:282). **Én treg lytter
  blokkerer opprydding for alle**, og feilmeldingen sier det rett ut (:2242):
  «The NOTIFY queue cannot be emptied until that process ends its current
  transaction.»

## Hvorfor mpedb ikke trenger det samme

Ikke fordi vi er smartere om låser — fordi vi ikke har objektet som krever dem.

Vi har ingen kø. Varselet bærer «tabell T er på generasjon G», ikke en payload,
og leseren har MVCC-snapshot til å hente resten selv. Da finnes det ingen delt
ordnet struktur å holde ordnet, ingen ucommittede poster som kan stå foran
committede, og ingenting å serialisere. N endringer mellom to vekkinger
koalescerer til én i stedet for å hope seg opp.

Og der Postgres regner ut «hvem bryr seg» ved commit — den må, kanalen er en
runtime-streng, `pg_notify(text, text)` kan regnes ut — er vår matching en
**kompileringstids-konstant**: hver setning er en forhåndskompilert plan med et
fotavtrykk, så hvilke spor den kan ringe på er kjent før den kjøres.

Prisen vi betaler for det: ingen total orden på tvers av urelaterte tabeller, og
ingen payload-leveranse. Begge er bevisste, og begge står i [benchmarken](../benchmarks/notify.md).
