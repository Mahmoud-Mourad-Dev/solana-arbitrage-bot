# تقرير مراجعة Codex — `solana-arbitrage-bot`

**تاريخ المراجعة:** 2026-07-18  
**الفرع:** `main`  
**الالتزام المراجع:** `9ae2d03f71c0b805d985fb282f2b81da20197511`  
**نطاق المراجعة:** مراجعة فقط دون تعديل الكود أو إنشاء commit أو تشغيل أي إجراء حي.

## 1. الملخص التنفيذي

تمت مراجعة المستودع عند الالتزام المتوقع، وكانت الشجرة المتتبعة نظيفة وقت المراجعة.

النتيجة الأساسية:

> تنفيذ Pump dynamic fee-v2 موصول فعليًا بمسار التسعير الحالي وبصورة صحيحة للحالات السليمة، لكن تقرير تشغيل الـ24 ساعة لا يمكن اعتباره موثوقًا بالكامل بصورته الحالية. التوصية هي إيقاف التشغيل وإصلاح مشكلات صحة القياس والتقرير قبل استخدام نتائجه لاتخاذ قرار اقتصادي.

لا يوجد دليل على قدرة `observe-narrow` على التوقيع أو الإرسال أو استخدام Jito. لكن توجد أربع مشكلات جوهرية:

1. ملف المسارات يحتوي مسارًا معلّمًا صراحةً بأنه غير آمن، ولا يقوم `observe-narrow` باستبعاده.
2. أخطاء RPC لا تُسجل كـpoll سلبي، مما قد يدمج حلقتين منفصلتين في episode واحدة ويشوّه الاستمرارية والتصنيف.
3. ملف JSONL لا يسجل تفاصيل Pump fee-v2 اللازمة لتدقيق الرسوم أو إعادة بناء التقرير الكامل.
4. سلامة snapshot تعتمد جزئيًا على عناوين من cache دون مطابقتها مع العناوين المفكوكة من حسابات pool/pair.

لا توجد نتائج موجبة معروفة في التشغيل الحالي بحسب حالة المشروع، ولذلك لا يوجد ربح وهمي ظاهر حاليًا. لكن إذا ظهرت episodes، فلا ينبغي اعتمادها اقتصاديًا قبل الإصلاح.

## 2. المعمارية الحالية

مسار `observe-narrow` الحالي هو:

1. قراءة `narrow-routes.feecorrect.json`.
2. المرور على 11 مسارًا بالتتابع في كل دورة.
3. جلب وقت الشبكة مرة واحدة في بداية sweep.
4. لكل مسار:
   - جلب Meteora pair أولًا لتحديد نافذة bin arrays.
   - تنفيذ `getMultipleAccounts` واحد يحتوي Pump pool/vaults، mint supply، fee config، Meteora pair وخمس bin arrays.
   - بناء Pump leg وMeteora leg.
   - تنفيذ Meteora WSOL→Token ثم Pump Token→WSOL.
   - حساب Pump tier من supply/reserves قبل الصفقة.
   - تشغيل optimizer على كائن Route نفسه.
   - كتابة `PollEvent` في JSONL.
5. عند اكتشاف episode، تحديد reconfirmations عند +2s و+10s و+30s.
6. كل reconfirm تجلب snapshot جديدة وتعيد optimization.
7. بناء التقرير الحي عبر `aggregate_narrow`.
8. أداة `rebuild-report` تقرأ `PollEvent` نفسها وتستدعي aggregator نفسها.

المسار الأساسي موحّد بالفعل:

- monitor والoptimizer وreconfirmation تصل جميعها إلى `Leg::quote_detailed`.
- Pump leg تستدعي حصريًا `pump_quote_detailed_v2`.
- لا يوجد fallback إلى 30 bps في هذا المسار.

المراجع:

- `monitor/src/route_engine.rs:78`
- `monitor/src/observe_live.rs:265`
- `monitor/src/bin/observe_narrow.rs:142`

## 3. النتائج مرتبة حسب الخطورة

### Critical

لا توجد مشكلة Critical مؤكدة تسمح بإرسال معاملة من `observe-narrow` أو تثبت وجود خطأ رياضي مباشر في Pump sell fee-v2.

### High

#### H1 — ملف التشغيل يحتوي مسارًا معلّمًا بأنه غير آمن

يحتوي `narrow-routes.feecorrect.json` على 11 مسارًا، أحدها:

- `safe=false`
- مسار USDC.
- غير مستبعد من الحلقة أو الأرقام الاقتصادية.

الملف نفسه يبين وجود mint authority وfreeze authority لهذا المسار في `narrow-routes.feecorrect.json:15`.

تمر الحلقة على جميع `cache.markets` دون فحص `m.safe` في `monitor/src/bin/observe_narrow.rs:126`.

**الأثر:** يمكن لمسار غير مصرح باعتباره آمنًا أن يدخل episode/economic totals، وهو ما يناقض وصف الملف بأنه curated safe route set.

#### H2 — فشل RPC قد يدمج episodes منفصلة

عند فشل `fetch_snapshot`، يقوم الكود بزيادة `rpc_failures` وحذف episode من `open`، لكنه لا يكتب PollEvent سلبيًا في JSONL: `monitor/src/bin/observe_narrow.rs:131`.

أما aggregator فيعتبر episode سلسلة من الـpolls الموجبة المتتالية، ولا يرى الفشل المفقود: `monitor/src/narrow_report.rs:226`.

مثال:

```text
positive poll → RPC failure → positive poll
```

التشغيل الحي يبدأ episode جديدة داخليًا، لكن إعادة البناء ترى episode واحدة مستمرة. وقد تلتصق reconfirmations بالـstart الجديد بينما aggregator تبحث عنها تحت start القديم.

**الأثر:**

- تضخيم مدة episode.
- تصنيف Flicker كـActive.
- تشويه عدد episodes ومعدلاتها.
- فقد أو سوء إسناد reconfirmations.
- اختلاف بين منطق التشغيل ومنطق التقرير.

#### H3 — التقرير لا يسجل أدلة fee-v2 المطلوبة

لا يحتوي `PollEvent` على:

- market cap.
- tier index.
- LP/protocol/creator bps.
- total bps أو قيمة كل fee component.
- hash/version لحساب fee config.
- هوية أو owner حساب config.

الحقول الفعلية محدودة بما يظهر في `monitor/src/narrow_report.rs:22`، والكتابة الفعلية للحدث لا تضيف هذه المعلومات: `monitor/src/bin/observe_narrow.rs:160`.

يتعارض ذلك أيضًا مع وعد runbook بأن التقرير يحتوي tier ديناميكيًا من كل snapshot: `docs/vps-fee-correct-observe-runbook.md:11`.

**الأثر:** لا يمكن إثبات بعد التشغيل أن كل observation استخدمت tier المتوقع، حتى لو كان التسعير الداخلي صحيحًا.

#### H4 — لا تُثبت مطابقة عناوين cache مع الحسابات المفكوكة

عناوين Pump vaults تأتي من cache وتُجلب مباشرة، لكن لا توجد مقارنة بين:

- `m.pump_base_vault` و`pool.base_vault`.
- `m.pump_quote_vault` و`pool.quote_vault`.
- `m.token_mint` و`pool.base_mint`.

المراجع:

- `monitor/src/observe_live.rs:156`
- `monitor/src/observe_live.rs:217`

كما لا يتم التحقق من owner حساب fee config أو mint رغم أن التعليقات تصف مصدر config بأنه authoritative: `monitor/src/observe_live.rs:99` و`monitor/src/observe_live.rs:214`.

**الأثر:** cache قديم أو خاطئ يمكن أن يركب pool state مع vault balances أو supply لا تخصه، وهذا مسار محتمل لربح وهمي.

### Medium

#### M1 — snapshot ليست كاملة وفق المتطلبات المعلنة

تشمل snapshot Pump state وMeteora pair/bin arrays، لكنها لا تشمل:

- Meteora reserve vault accounts.
- oracle account.
- bitmap-extension account.
- التحقق من vault mint/authority.

قائمة الحسابات الفعلية موجودة في `monitor/src/observe_live.rs:183`.

محرك DLMM يعتمد على bitmap المضمنة داخل pair ويرفض ما يقع خارجها: `monitor/src/meteora_dlmm.rs:131`.

غياب bitmap extension محافظ غالبًا لأنه يؤدي إلى رفض بدل overquote، لكنه يجعل التغطية ناقصة. أما غياب تحقق vault provenance فهو مشكلة صحة فعلية مرتبطة بـH4.

#### M2 — يوجد pair probe خارج snapshot الرئيسية

يُجلب pair مرة لاختيار bin-array window، ثم يُجلب مرة ثانية مع snapshot: `monitor/src/observe_live.rs:164`.

الحالة المستخدمة في quote تأتي من الطلب الثاني، لذلك لا يخلط ذلك قيمة active ID القديمة مباشرة مع pair قديمة. لكن قد تتحرك active bin بين الطلبين، فتكون نافذة bin arrays غير مناسبة.

السلوك المتوقع هو رفض محافظ بسبب missing bins وليس ربحًا وهميًا، لكنه قد يفقد فرصًا صحيحة ويرفع معدلات الفشل.

#### M3 — وقت Meteora ليس من slot نفسها

يُجلب `cluster_time` قبل المرور التسلسلي على المسارات ثم يُعاد استخدامه: `monitor/src/bin/observe_narrow.rs:125`.

وعند reconfirmation يُمرر `now_unix` القديم نفسه: `monitor/src/bin/observe_narrow.rs:214`.

الوقت يؤثر على DLMM volatility decay. التأثير غالبًا صغير خلال sweep قصيرة، لكنه يعني أن كل عناصر quote ليست من context slot نفسها إذا طال sweep أو حدث timeout.

#### M4 — optimizer يفترض ضمنيًا منحنى مناسبًا للبحث الثلاثي

يستخدم البحث المحلي ternary search: `monitor/src/optimizer.rs:101`.

Pump tier ثابت عبر أحجام الصفقة لأنه pre-trade، وهذا صحيح. لكن net cost يتضمن Jito tip steps، وDLMM يحتوي bin crossings، ولذلك الدالة ليست مضمونة unimodal.

الكود لا يضيف probes صريحة حول:

- كل Jito threshold.
- كل fee/cost discontinuity.
- كل DLMM bin-capacity boundary.

`size_analysis` يكتشف القرب من Jito tier بعد coarse samples فقط ولا يصحح الاختيار: `monitor/src/optimizer.rs:249`.

الخطر الأرجح هو missed opportunity أو underestimation وليس overquote، لأن الحجم النهائي يعاد تقييمه على Route نفسها في `monitor/src/optimizer.rs:216`.

#### M5 — إعادة بناء التقرير ليست مطابقة كاملة أو byte-identical

الأداة تستخدم aggregator نفسها، لكن:

- يجب تمرير `--routes` و`--controls` يدويًا.
- القيمة الافتراضية للـcontrols فارغة، بينما التشغيل الحي له control افتراضي.
- التقرير المعاد لا يحتوي cadence أو RPC failures أو sweep timings أو optimizer correction.
- `commit` هو commit وقت إعادة البناء وليس بالضرورة commit وقت التسجيل.

المراجع:

- `monitor/src/bin/rebuild_report.rs:99`
- `monitor/src/bin/rebuild_report.rs:136`

لذلك metrics الأساسية قد تتطابق عند تمرير الخيارات الصحيحة، لكن التقرير النهائي الكامل ليس byte-identical.

#### M6 — decoder لا يفرض schema ثابتة بالكامل

يبحث `decode_fee_config` عن أول run بنيوي من 16 tier على الأقل بدل فرض offset/count/version ثابتة: `monitor/src/pump_feev2.rs:65`.

الحساب المعروف يحتوي 24 tier، لكن decoder قد يقبل جدولًا مبتورًا بعد 16 tier أو bytes مصادفة تحقق النمط قبل الجدول الحقيقي. كما لا يوجد upper bound لقيم creator bps.

هذا لا يضر fixture الحالية، لكنه يضعف معالجة unsupported-version وmalformed config.

#### M7 — لا توجد timeouts أو backoff صريحة في الحلقة

تُستدعى المسارات بالتتابع: `monitor/src/bin/observe_narrow.rs:126`.

لا يوجد `tokio::time::timeout` حول RPC ولا backoff حسب معدل الفشل. لذلك:

- RPC بطيئة قد تجعل sweep تتجاوز ثلاث ثوانٍ.
- كل route بطيء يؤخر بقية المسارات.
- reconfirmations تزيد التأخير داخل الحلقة نفسها.
- لا يوجد isolation بالتوازي بين المسارات.

sleep scheduling نفسه صحيح؛ ينام بقية target period فقط: `monitor/src/bin/observe_narrow.rs:249`.

### Low

#### L1 — لا توجد heartbeat دورية قصيرة

السجل يطبع startup وhourly checkpoint وshutdown/final output فقط:

- `monitor/src/bin/observe_narrow.rs:98`
- `monitor/src/bin/observe_narrow.rs:255`

إذا لم تمر ساعة، فوجود startup فقط متوقع. بعد أكثر من ساعة يجب ظهور hourly checkpoint. يوصى مستقبلًا بـheartbeat كل 1–5 دقائق تتضمن sweep count وآخر slot وfailure rate وحجم JSONL.

#### L2 — الذاكرة تنمو طوال التشغيل

كل PollEvent وكل sweep/sleep sample يبقى في الذاكرة: `monitor/src/bin/observe_narrow.rs:112`.

الحجم المتوقع لـ11 مسارًا خلال 24 ساعة قابل للتحمل غالبًا، لكنه غير محدود تصميميًا.

#### L3 — JSONL flush جيد لكن report write ليس atomic

يتم `flush` بعد كل sweep: `monitor/src/bin/observe_narrow.rs:247`.

لكن checkpoint يُكتب مباشرة إلى الملف نفسه عبر `std::fs::write`: `monitor/src/bin/observe_narrow.rs:409`. قد يترك انقطاع العملية report مبتورًا، بينما تظل JSONL أفضل مصدر للاستعادة.

#### L4 — وثائق قديمة ومتعارضة

يحتوي `pump_amm.rs` شرحًا بارزًا يقول إن الرسوم دائمًا 30 bps قبل ملاحظة لاحقة توضح fee-v2: `monitor/src/pump_amm.rs:35`.

كما يصف `docs/current-architecture.md` غياب modes صريحة رغم وجودها الآن، مما يزيد خطر استخدام مسار legacy بطريق الخطأ.

## 4. مراجعة Pump fee-v2

الأجزاء الصحيحة:

- المصدر الثابت المعروف للحساب موجود في `monitor/src/observe_live.rs:102`.
- discriminator معروف والإصدارات غير المدعومة تُرفض: `monitor/src/pump_feev2.rs:70`.
- market cap تستخدم `u128` وفق المعادلة `supply × quote_reserve / base_reserve`: `monitor/src/pump_feev2.rs:120`.
- tier المختارة هي أعلى threshold لا تتجاوز market cap.
- المطابقة عند threshold تستخدم `<=`.
- الرسوم LP/protocol/creator تُقرب كل واحدة للأعلى بصورة مستقلة.
- لا يوجد fallback متفائل إلى 30 bps.
- Pump route الفعلية تستدعي v2 فقط.
- الحجم النهائي يعاد تقييمه بالمحرك نفسه.
- الاختبارات تثبت Route 1 عند 75 bps وRoute 3 عند 95 bps.

تعتمد tier على **pre-trade state** وتبقى ثابتة لكل أحجام optimizer داخل snapshot واحدة: `monitor/src/pump_amm.rs:335`. يتوافق ذلك مع evidence والاختبارات الموجودة.

التحفظات:

- owner الحساب غير مفحوص.
- config version/hash غير مسجل.
- decoder يقبل run جزئيًا من 16 tier.
- تفاصيل tier المحسوبة لا تنتقل إلى Candidate/PollEvent؛ يعيد `PumpQuoteDetail` فقط out وfee.

**الخلاصة:** Pump fee-v2 موصولة رياضيًا بشكل صحيح في مسار Pump sell، لكنها ليست موصولة بالكامل من ناحية provenance/reporting/auditability.

## 5. الحلقات والمقاييس الاقتصادية

الجيد:

- detection value هي أول poll في الحلقة.
- hindsight maximum معلّمة بوضوح كحد أعلى غير سببي.
- الاقتصاد يحتسب قيمة واحدة عند detection لكل episode.
- frozen route لا تُصنف FrozenSpread إذا كان لها أي episode؛ تصبح Flicker أو Active.
- controls تُستبعد من totals إذا تم التعرف على token بصورة صحيحة.

المراجع:

- `monitor/src/narrow_report.rs:263`
- `monitor/src/narrow_report.rs:305`
- `monitor/src/narrow_report.rs:334`

المشكلات:

- لا يوجد metric مستقل باسم first-subsequent-poll value.
- النوافذ واسعة؛ +2s تعني عينة بين 1 و6 ثوانٍ، و+10s بين 6 و20 ثانية، و+30s بين 20 و120 ثانية: `monitor/src/narrow_report.rs:252`.
- `survived` يعد `net >= 0`، لكن reconfirm الفاشلة تكتب net=0 و`profitable=false`. مع ذلك تستخرج milestone قيمة net فقط، وبالتالي قد تُحسب الفشل كنجاة لأن `0 >= 0`.

المراجع:

- `monitor/src/observe_live.rs:328`
- `monitor/src/narrow_report.rs:349`

قد تكون أرقام survival متضخمة حتى دون ربح فعلي، ويجب الاعتماد على `profitable_competitive` أو تخزين `Option<net>` للفشل.

## 6. تدقيق السلامة

`observe-narrow` نفسه:

- لا يعتمد على executor.
- لا يحمل keypair.
- لا يبني transaction.
- لا يوقع.
- لا يحاكي.
- لا يستدعي Jito أو Redis.
- لا يحتوي مسار submission.

المسارات القادرة على الإرسال موجودة في `executor` و`bot` فقط:

- تحميل keypair: `executor/src/app.rs:54`.
- البناء والتوقيع: `executor/src/app.rs:171`.
- بوابة الإرسال: `executor/src/app.rs:188`.
- `send_bundle`: `executor/src/app.rs:211`.

كان الملف `.live-armed` غير موجود محليًا وقت المراجعة، وهو مضاف إلى `.gitignore` في `.gitignore:15`.

لم يتم فتح `.env`، لذلك لا يمكن إثبات القيم الفعلية على الـVPS. ومع ذلك، حتى لو كانت متغيرات البيئة غير آمنة، لا يمتلك binary `observe-narrow` مسار إرسال. السلامة هنا ناتجة عن الفصل البنيوي، لا عن flags فقط.

## 7. الأوامر والاختبارات المنفذة

```bash
git status --short --branch
git rev-parse --verify HEAD
git log -5 --oneline --decorate
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

النتائج:

- `cargo fmt`: ناجح.
- `cargo clippy`: ناجح دون تحذيرات.
- `cargo test`: ناجح بالكامل.
- 196 اختبارًا فعليًا نجحت، دون أي فشل.
- لم يتم تشغيل observe طويل أو simulation أو submission أو Jito أو deployment.

التغطية الجيدة تشمل:

- fee tier boundaries.
- discriminator خاطئ.
- zero reserves/supply.
- Pump parity.
- no optimistic fallback.
- optimizer basics/capacity.
- report aggregation.
- mode arming.
- no-sign/no-send symbol scans.

التغطية الناقصة أو غير الكافية تشمل:

- truncated-but-structurally-valid 16-tier configs.
- creator bps غير معقولة أو overflow.
- cache vault/mint mismatch.
- RPC gap بين pollين موجبين.
- reconfirm failure ذات net=0.
- offline/live full-report equivalence.
- اختبار تكامل حقيقي لـSIGINT/SIGTERM.
- timeout/backoff.
- unsafe route exclusion.
- Jito step boundaries وDLMM discontinuities في optimizer.

## 8. هل يمكن الوثوق بتشغيل الـ24 ساعة؟

**لا، ليس كتقرير اقتصادي نهائي.**

يمكن الوثوق نسبيًا بأن كل Pump sell quote ناجحة استخدمت fee-v2 بدل 30 bps. لكن لا يمكن الوثوق في:

- episode count/lifetime عند وجود RPC failures.
- survival counts.
- استبعاد كل المسارات غير الآمنة.
- القدرة على تدقيق tier لكل observation.
- offline reconstruction الكامل.
- خلو كل quote من cache provenance mismatch.

إذا ظل التشغيل دون أي episodes، فالاستنتاج المحدود «لم يرصد البوت فرصة موجبة» معقول، لكنه غير كافٍ للحكم على الاقتصاد بسبب احتمال missed polls/timeouts والمسار غير الآمن.

## 9. هل ما زال الربح الوهمي ممكنًا؟

نعم، عبر:

1. عدم مطابقة pool مع vaults/mint القادمة من cache.
2. دمج episodes عبر RPC gaps.
3. احتساب reconfirm فاشلة ذات net=0 كنجاة.
4. إدخال مسار `safe=false` في الاقتصاد.
5. عدم وجود سجل fee provenance يسمح بإثبات tier لاحقًا.

أما pair probe خارج snapshot فقد يؤدي غالبًا إلى missed opportunity وليس phantom profit.

## 10. الإصلاحات المقترحة بالترتيب

1. استبعاد أي `m.safe != true` عند تحميل cache، أو رفض التشغيل بالكامل إن وجد.
2. تسجيل PollEvent صريحة عند كل فشل أو رفض، مع `valid_snapshot=false` وسبب typed، وفصل episode عند gap.
3. إصلاح survival ليعتمد على `profitable_competitive && net >= 0`، لا net وحدها.
4. التحقق من pool vault addresses وpool base mint وtoken-account mint/owner وmint owner وfee-config owner وهويات Meteora reserves.
5. إضافة fee-v2 provenance إلى snapshot/event: market cap وtier index وcomponent bps والرسوم الفعلية وconfig pubkey/owner/hash/schema version.
6. جعل إعادة البناء تلتقط metadata/config من JSONL نفسها وتنتج metrics نفسها دون flags خارجية.
7. تثبيت decoder على schema معروفة: offset/count=24، bounds للـbps، وفشل صريح عند أي truncation غير متوقعة.
8. إضافة probes صريحة حول Jito thresholds وDLMM capacity/bin boundaries.
9. إضافة timeouts وbounded backoff وper-route failure isolation.
10. إضافة heartbeat قصيرة وكتابة report ذريًا عبر temporary file ثم rename.
11. تحديث الوثائق التي لا تزال تصف رسوم 30 bps أو بنية قديمة.

## 11. القرار النهائي

**أوقف التشغيل وأصلح مشكلات الصحة أولًا.**

ليس لأن Pump fee-v2 الحسابية خاطئة، بل لأن القياس المحيط بها يمكن أن ينتج تقريرًا مضللًا، خصوصًا في episode segmentation وsurvival والمسار غير الآمن وغياب fee provenance.

أفضل توصيف للحالة الحالية:

> Pump dynamic fee-v2 صحيحة ومستخدمة في quote path، لكن تشغيل الـ24 ساعة غير صالح بعد كدليل اقتصادي نهائي، وما زال احتمال phantom أو misclassified profit قائمًا.
