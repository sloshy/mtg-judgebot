---
title: Sample answers
description: What the judge's answers look like, as screenshots of the web page and five answers copied verbatim from a published evaluation run.
sidebar:
  order: 3
---

The following are sample answers given by the bot so you can see how it works without running it yourself.

![The web page answering a question about Dark Confidant and Tarmogoyf: the ruling, six linked citations, the cards the question was resolved to, and the confidence.](../../../assets/screenshots/web-answer-light.png)

A nickname that could mean several cards gets a question back instead of a guess:

![The web page asking which Tibalt was meant, with three cards to choose from.](../../../assets/screenshots/web-did-you-mean.png)

On Discord the answer arrives as a message, with the citations in an embed and the rating
buttons beneath it. The "did you mean…?" choices are buttons too.

![A Discord reply to /judge. It restates the question about revealing an X-cost spell to Dark Confidant and answers in two paragraphs that X counts as 0, with a rule number inline and mana symbols drawn as pictures. An embed quotes a dated Dark Confidant ruling, the card's Oracle text and rule 107.3g, and ends with "Cards: Dark Confidant", the confidence and the CR version. Three buttons follow: Incorrect, Partially correct, Correct.](../../../assets/screenshots/discord-answer-citations.png)

![A Discord reply asking which Tibalt was meant: a numbered list of three cards, and a button for each.](../../../assets/screenshots/discord-did-you-mean.png)

## Where these come from

The five answers below are copied from `eval/published/v1-opus-5-5.json`, the 2026-09-22
run of the [gold evaluation set](../../how-it-works/evaluation/) on the default
configuration (`claude-opus-5-5` on both stages, Voyage embeddings). The run file holds all
21, including the one where the pipeline asked which card was meant instead of answering
and the one it refused to show.

Nothing is edited except the one note on the last sample. Each question is as the gold
set asks it, typos and all. The answer, citations, time and cost are as the run recorded
them.

Every quote was checked verbatim against its source before the answer was accepted. The
run file cuts long quotes at a display limit, which is the "…" at the end of a few below.
Validation checked the full span. Rule numbers link to the Yawgatog mirror, as they do in
a real answer.

## Layers: Blood Moon and Urborg

**Asked**

> follow up: what if I play [[Urborg, Tomb of Yawgmoth]] AFTER the moon is already down? urborg has the later timestamp so shouldn't it make everything a Swamp too? my friend says no

**Answered**

> Your friend is right. Even if **Urborg, Tomb of Yawgmoth** comes down after **Blood Moon**, nothing becomes a Swamp. Under the Moon, Urborg is just a Mountain with no ability of its own.
>
> Why the timestamp doesn't matter: both effects change land types, so both apply in layer 4 (`613.1d`). Normally effects in the same layer apply in timestamp order, but per `613.8`, "If a dependency exists, it will override the timestamp system." Urborg's effect depends on Blood Moon's under `613.8a`, because applying Blood Moon "would change the text or the existence of the first effect." Urborg is a nonbasic land, so Blood Moon ("Nonbasic lands are Mountains.") turns it into a Mountain. Per `305.7`, that means it "loses all abilities generated from its rules text," and that includes "Each land is a Swamp in addition to its other land types."
>
> Per `613.8b`, the dependent effect (Urborg's) waits until Blood Moon's effect has been applied. By then Urborg no longer has the ability, so its effect never applies. Scryfall's ruling on Urborg confirms this with Magus of the Moon, which works the same way: Urborg "won't turn lands into Swamps, no matter in what order those effects started to apply."
>
> Result: your nonbasic lands, Urborg included, are Mountains that tap for {R}. Blood Moon doesn't touch your basic lands, so they keep their normal types, and none of them become Swamps.

- [**613.8**](https://yawgatog.com/resources/magic-rules/#R6138) “If a dependency exists, it will override the timestamp system.”
- [**613.8a**](https://yawgatog.com/resources/magic-rules/#R6138a) “applying the other would change the text or the existence of the first effect”
- [**613.8b**](https://yawgatog.com/resources/magic-rules/#R6138b) “An effect dependent on one or more other effects waits to apply until just after all of those effects have been applied.”
- [**305.7**](https://yawgatog.com/resources/magic-rules/#R3057) “It loses all abilities generated from its rules text, its old land types, and any copiable effects affecting that land, and it gains the appropriate mana ability for each new basic land type.”
- [**613.1d**](https://yawgatog.com/resources/magic-rules/#R6131d) “Layer 4: Type-changing effects are applied.”
- **Ruling — Urborg, Tomb of Yawgmoth** “it won't turn lands into Swamps, no matter in what order those effects started to apply.”
- **Oracle text — Blood Moon** “Nonbasic lands are Mountains.”

Confidence: High · CR 2026-08-19 · 49 s · 3 model calls · $0.13

## Trample and deathtouch, and a misremembered card

**Asked**

> I attack with a 5/5 trample that also has deathtouch (Questing Beast) and they block with a 4/4. how much can I trample over? my opponent says I have to assign 4 to the blocker first

**Answered**

> Your opponent is wrong here. With deathtouch and trample, you only have to assign **1** damage to the 4/4. The other **4** can go to the player (or you can split it however you like).
>
> Why: under `702.19b`, a trampler first assigns damage to its blockers, and "once all those blocking creatures are assigned lethal damage, any excess damage" can go to the player it's attacking. Deathtouch changes what counts as lethal: per `702.2c`, "Any nonzero amount of combat damage assigned to a creature by a source with deathtouch is considered to be lethal damage." So 1 damage fully covers the 4/4, and deathtouch still destroys it (`702.2b`).
>
> Two side notes:
> - **Questing Beast**'s current Oracle text is a 4/4 with "Vigilance, deathtouch, haste" and no trample. So the trample and the extra +1/+1 have to come from something else you have. Without trample, all 5 damage goes to the blocker (`510.1c`).
> - If the damage that tramples over hits your opponent, Beast's last ability deals that much damage again to target planeswalker they control. "Combat damage that would be dealt by creatures you control can't be prevented" also applies.

- [**702.19b**](https://yawgatog.com/resources/magic-rules/#R70219b) “Once all those blocking creatures are assigned lethal damage, any excess damage is assigned as its controller chooses among those blocking creatures and the player, planeswalker, or battle the creatur…”
- [**702.2c**](https://yawgatog.com/resources/magic-rules/#R7022c) “Any nonzero amount of combat damage assigned to a creature by a source with deathtouch is considered to be lethal damage for the purposes of determining if excess damage is being dealt.”
- [**702.2b**](https://yawgatog.com/resources/magic-rules/#R7022b) “A creature with toughness greater than 0 that’s been dealt damage by a source with deathtouch since the last time state-based actions were checked is destroyed as a state-based action.”
- [**510.1c**](https://yawgatog.com/resources/magic-rules/#R5101c) “If exactly one creature is blocking it, it assigns all its combat damage to that creature.”
- **Oracle text — Questing Beast** “Vigilance, deathtouch, haste”

Confidence: High · CR 2026-08-19 · 16 s · 3 model calls · $0.10

## An old printing against the current Oracle text

**Asked**

> old school question. my Waylay (Urza's Saga printing) says 'at end of turn, remove them from the game'. so I cast it during my opponent's end step, the knights survive since the end step already started, and I get to swing with them on my turn right? classic waylay trick

**Answered**

> No, that trick doesn't work anymore. **Waylay** has Oracle errata, and its current text is: "Create three 2/2 white Knight creature tokens. Exile them at the beginning of the next cleanup step." The old "at end of turn" wording is gone, and your Urza's Saga printing plays by the current Oracle text.
>
> When Waylay resolves, it creates a delayed triggered ability (`603.7`) that waits for the next cleanup step. That's the very next step of the same turn, because "The ending phase consists of two steps: end and cleanup" (`512.1`). When that cleanup step starts, the game checks whether any triggered abilities are waiting to go on the stack, "including those that trigger 'at the beginning of the next cleanup step'" (`514.3a`). The ability goes on the stack, players get priority, it resolves, and the Knights are exiled. Then another cleanup step begins.
>
> So if you cast it in your opponent's end step, the Knights only last for the rest of that end step. They can still be sacrifice fodder, or surprise blockers if you cast Waylay earlier in the turn, but they never make it to your turn.

- **Oracle text — Waylay** “Create three 2/2 white Knight creature tokens. Exile them at the beginning of the next cleanup step.”
- [**603.7**](https://yawgatog.com/resources/magic-rules/#R6037) “An effect may create a delayed triggered ability that can do something at a later time.”
- [**512.1**](https://yawgatog.com/resources/magic-rules/#R5121) “The ending phase consists of two steps: end and cleanup.”
- [**514.3a**](https://yawgatog.com/resources/magic-rules/#R5143a) “At this point, the game checks to see if any state-based actions would be performed and/or any triggered abilities are waiting to be put onto the stack (including those that trigger “at the beginning …”

Confidence: High · CR 2026-08-19 · 13 s · 3 model calls · $0.09

## Commander: colour identity and the command zone

**Asked**

> commander q: my commander is [[Kenrith, the Returned King]]. two things: 1) can I run Nicol Bolas, the Ravager since Kenrith's text mentions all 5 colors? and 2) someone bounced Kenrith to my hand with Unsummon - does he go to the command zone or is that only for graveyard/exile?

**Answered**

> 1) Yes. **Kenrith**'s color identity is all five colors, so **Nicol Bolas, the Ravager** is legal in your deck. 2) You choose. When Kenrith would go to your hand, you may put him into the command zone instead. This option isn't limited to the graveyard and exile.
>
> **Color identity:** Per `903.4`, a card's color identity counts the mana symbols in its mana cost *and* in its rules text. Kenrith costs {4}{W}, but his abilities include {R}, {1}{G}, {2}{W}, {3}{U} and {4}{B}. That gives him white, red, green, blue and black. Per `903.5c`, a card can go in your deck only if every color in its identity is also in your commander's identity. Bolas's identity is blue, black and red. His back face is included per `903.4d`, and it's the same three colors. All of those are inside Kenrith's five, so Bolas is fine.
>
> **Unsummon:** Per `903.9b`, if a commander would be put into its owner's hand or library from anywhere, the owner may put it into the command zone instead. This is a replacement effect, so you decide as **Unsummon** resolves. Take him to your hand if you'd rather recast him from there without the commander tax. Or send him to the command zone. The separate rule for the graveyard and exile (`903.9a`) is a state-based action. It doesn't cover your hand.

- [**903.4**](https://yawgatog.com/resources/magic-rules/#R9034) “The color identity of a card is the color or colors of any mana symbols in that card’s mana cost or rules text”
- [**903.5c**](https://yawgatog.com/resources/magic-rules/#R9035c) “A card can be included in a Commander deck only if every color in its color identity is also found in the color identity of the deck’s commander.”
- [**903.4d**](https://yawgatog.com/resources/magic-rules/#R9034d) “The back face of a double-faced card (see rule 712) is included when determining a card’s color identity.”
- [**903.9b**](https://yawgatog.com/resources/magic-rules/#R9039b) “If a commander would be put into its owner’s hand or library from anywhere, its owner may put it into the command zone instead.”
- [**903.9a**](https://yawgatog.com/resources/magic-rules/#R9039a) “If a commander is in a graveyard or in exile and that object was put into that zone since the last time state-based actions were checked, its owner may put it into the command zone.”
- **Oracle text — Kenrith, the Returned King** “{4}{B}: Put target creature card from a graveyard onto the battlefield under its owner's control.”

Confidence: High · CR 2026-08-19 · 13 s · 2 model calls · $0.10

## Out of scope: a price question

**Asked**

> how much is a snappy going for these days? thinking about picking up a playset

**Answered**

> I only answer Comprehensive Rules and Commander rules questions. Tournament policy (MTR/IPG) and questions that aren't about the rules are out of my scope: for policy, see the Magic Tournament Rules or ask a tournament judge.

(The run file records this outcome as `OutOfScope`. The sentence above is what Discord
and the web page show for it.)

Declined after the classification call, before synthesis · 2 s · 1 model call · $0.002
