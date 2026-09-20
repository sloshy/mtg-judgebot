---
title: Sample answers
description: What the judge's answers look like, as screenshots of the web page and five answers copied verbatim from a published evaluation run.
sidebar:
  order: 3
---

No judgebot is offered to try, so this page is the look before you run one: the web page
as it renders, then five answers exactly as the pipeline gave them.

![The web page answering a question about Dark Confidant and Tarmogoyf: the ruling, six linked citations, the cards the question was resolved to, and the confidence.](../../../assets/screenshots/web-answer-light.png)

A nickname that could mean several cards gets a question back instead of a guess:

![The web page asking which Tibalt was meant, with three cards to choose from.](../../../assets/screenshots/web-did-you-mean.png)

On Discord the answer arrives as a message, with the citations in an embed and the rating
buttons beneath it. The "did you mean…?" choices are buttons too.

![A Discord reply to /judge. It restates the question about revealing an X-cost spell to Dark Confidant, answers that X counts as 0, and cites the card's Oracle text, rule 202.3e and a Dark Confidant ruling in an embed that ends with the confidence and CR version. Three buttons follow: Incorrect, Partially correct, Correct.](../../../assets/screenshots/discord-answer-citations.png)

## Where these come from

The five answers below are copied from `eval/published/v1-opus-5.json`, the 2026-09-20
run of the [gold evaluation set](../../how-it-works/evaluation/) on the default
configuration (`claude-opus-5` on both stages, Voyage embeddings). Nothing is edited, bar the one note on the last sample:
the question is as the gold set asks it, typos and all, and the answer, the citations,
the time and the cost are as the run recorded them. Every quote was checked verbatim
against its source before the answer was accepted. Rule numbers link to the Yawgatog
mirror, as they do in a real answer. The run file holds all 21, including the one the
pipeline failed to answer.

## Layers: Humility and Opalescence

**Asked**

> ok classic one. I have [[Humility]] out and my opponent drops [[Opalescence]]. what are Humility and Opalescence now?? are all my creatures still 1/1? does the answer change if Opalescence was out first?

**Answered**

> Your board: **Humility** was out first, so it ends up a 4/4 enchantment creature, **Opalescence** stays a noncreature enchantment with no power/toughness, and your ordinary creatures are 1/1 with no abilities.
>
> Walking the layers: layer 4, Opalescence's "Each other non-Aura enchantment is a creature in addition to its other types" animates Humility (and any other non-Aura enchantments) — but it "Does not animate itself," so Opalescence isn't a creature and keeps its ability, since Humility only strips abilities from creatures. Layer 6, Humility's "All creatures lose all abilities" applies to everything that's now a creature, including Humility itself; it keeps applying in later layers even though it removed its own ability, per `613.6`. Layer 7b, both "base power and toughness 1/1" and "base power and base toughness each equal to its mana value" are P/T-setting effects in the same sublayer (`613.4b`), so the later timestamp wins (`613.7`). Humility is older, so Opalescence's setting applies last: Humility and any other animated enchantment are 4/4 (mana value 4). Normal creatures aren't touched by Opalescence, so they stay 1/1.
>
> Reverse the order and yes, it changes: if Opalescence was already out and Humility arrives later, Humility's 1/1 has the later timestamp, so every creature — including the animated enchantments and Humility itself — is 1/1 with no abilities. Opalescence itself is still a noncreature enchantment either way.

- **Oracle text — Opalescence** “Each other non-Aura enchantment is a creature in addition to its other types and has base power and base toughness each equal to its mana value.”
- **Oracle text — Humility** “All creatures lose all abilities and have base power and toughness 1/1.”
- [**613.4b**](https://yawgatog.com/resources/magic-rules/#R6134b) “Layer 7b: Effects that set power and/or toughness to a specific number or value are applied.”
- [**613.6**](https://yawgatog.com/resources/magic-rules/#R6136) “If an effect starts to apply in one layer and/or sublayer, it will continue to be applied to the same set of objects in each other applicable layer and/or sublayer, even if the ability generating the …”
- [**613.7**](https://yawgatog.com/resources/magic-rules/#R6137) “An effect with an earlier timestamp is applied before an effect with a later timestamp.”
- **Ruling — Humility** “Humility becomes 1/1 and Worship becomes 1/1 (Humility). Humility becomes 4/4 and Worship becomes 4/4 (Opalescence).”
- **Ruling — Opalescence** “Does not animate itself.”
- **Ruling — Opalescence** “If Humility has the earliest timestamp, then everything is 4/4.”

Confidence: High · CR 2026-08-19 · 61 s · 3 model calls · $0.22

## Trample and deathtouch, with a card that has neither… quite

**Asked**

> I attack with a 5/5 trample that also has deathtouch (Questing Beast) and they block with a 4/4. how much can I trample over? my opponent says I have to assign 4 to the blocker first

**Answered**

> Two corrections first: **Questing Beast** is a 4/4, not a 5/5, and it has no trample — its Oracle text reads "Vigilance, deathtouch, haste" plus the can't-be-blocked-by-power-2-or-less, damage-can't-be-prevented, and planeswalker-damage abilities. With no trample, a blocked Questing Beast assigns all 4 of its combat damage to the single blocker and nothing goes through: "If exactly one creature is blocking it, it assigns all its combat damage to that creature" (`510.1c`).
>
> If the attacker did have trample alongside deathtouch, your opponent would be wrong: you assign lethal damage to the blockers first, then the excess where you like (`702.19b`), and with deathtouch "Any nonzero amount of combat damage assigned to a creature by a source with deathtouch is considered to be lethal damage for the purposes of determining if excess damage is being dealt" (`702.2c`). So a 5/5 trample+deathtouch attacker blocked by a 4/4 assigns just 1 to the blocker and 4 to the player.

- **Oracle text — Questing Beast** “Vigilance, deathtouch, haste”
- [**510.1c**](https://yawgatog.com/resources/magic-rules/#R5101c) “If exactly one creature is blocking it, it assigns all its combat damage to that creature.”
- [**702.19b**](https://yawgatog.com/resources/magic-rules/#R70219b) “The controller of an attacking creature with trample first assigns damage to the creature(s) blocking it. Once all those blocking creatures are assigned lethal damage, any excess damage is assigned as…”
- [**702.2c**](https://yawgatog.com/resources/magic-rules/#R7022c) “Any nonzero amount of combat damage assigned to a creature by a source with deathtouch is considered to be lethal damage for the purposes of determining if excess damage is being dealt.”

Confidence: High · CR 2026-08-19 · 19 s · 3 model calls · $0.13

## Nicknames: bob, tim and goyf

**Asked**

> my opp has [[bob]] and flipped a tim off the top, then next turn he flipped a goyf. how much life does he lose each time? goyf is like a 5/6 rn so is it 5?

**Answered**

> Three, then two. Mana value is the total mana in the card's mana cost (`202.3`): **Prodigal Sorcerer** costs {2}{U}, so mana value 3; **Tarmogoyf** costs {1}{G}, so mana value 2 — no matter how big it is on the battlefield. Tarmogoyf's characteristic-defining ability ("Tarmogoyf's power is equal to the number of card types among cards in all graveyards and its toughness is equal to that number plus 1") sets power and toughness, which has nothing to do with its mana cost (`208.2a`). So **Dark Confidant** has him "reveal the top card of your library and put that card into your hand. You lose life equal to its mana value" — 3 life for the Tim, 2 for the Goyf.

- [**202.3**](https://yawgatog.com/resources/magic-rules/#R2023) “The mana value of an object is a number equal to the total amount of mana in its mana cost, regardless of color.”
- [**208.2a**](https://yawgatog.com/resources/magic-rules/#R2082a) “The card may have a characteristic-defining ability that sets its power and/or toughness according to some stated condition.”
- **Oracle text — Tarmogoyf** “Tarmogoyf's power is equal to the number of card types among cards in all graveyards and its toughness is equal to that number plus 1.”
- **Oracle text — Prodigal Sorcerer** “{T}: This creature deals 1 damage to any target.”
- **Oracle text — Dark Confidant** “reveal the top card of your library and put that card into your hand. You lose life equal to its mana value.”

Confidence: High · CR 2026-08-19 · 15 s · 2 model calls · $0.12

## Commander: colour identity and the command zone

**Asked**

> commander q: my commander is [[Kenrith, the Returned King]]. two things: 1) can I run Nicol Bolas, the Ravager since Kenrith's text mentions all 5 colors? and 2) someone bounced Kenrith to my hand with Unsummon - does he go to the command zone or is that only for graveyard/exile?

**Answered**

> Both answers are yes.
>
> 1) **Kenrith, the Returned King**'s color identity is all five colors: color identity is "the color or colors of any mana symbols in that card's mana cost or rules text" (`903.4`), and Kenrith's rules text includes "{R}: All creatures gain trample and haste until end of turn." along with the {1}{G}, {2}{W}, {3}{U} and {4}{B} activation costs, on top of his white mana cost — WUBRG. **Nicol Bolas, the Ravager // Nicol Bolas, the Arisen** is blue-black-red (front face's mana cost, plus the back face's color indicator, which counts per `903.4d`), so every color in its identity is in Kenrith's and it's legal under `903.5c`. A five-color commander lets you run essentially anything.
>
> 2) Command zone, if you want it there. The graveyard/exile state-based action in `903.9a` isn't the only route: `903.9b` says "If a commander would be put into its owner's hand or library from anywhere, its owner may put it into the command zone instead." **Unsummon** returning Kenrith to your hand is exactly that event, so you may apply the replacement — he'll then cost {2} more the next time you cast him from there per `903.8`. You may also just let him go to your hand.

- **Ruling — Kenrith, the Returned King** “Kenrith's first ability affects only creatures on the battlefield at the time it resolves.”
- **Oracle text — Kenrith, the Returned King** “{R}: All creatures gain trample and haste until end of turn.”
- [**903.4**](https://yawgatog.com/resources/magic-rules/#R9034) “The color identity of a card is the color or colors of any mana symbols in that card’s mana cost or rules text”
- [**903.4d**](https://yawgatog.com/resources/magic-rules/#R9034d) “The back face of a double-faced card (see rule 712) is included when determining a card’s color identity.”
- **Ruling — Nicol Bolas, the Ravager // Nicol Bolas, the Arisen** “Nicol Bolas, the Arisen has a color indicator on its typeline. This color indicator means that it's a blue, black, and red permanent.”
- [**903.5c**](https://yawgatog.com/resources/magic-rules/#R9035c) “A card can be included in a Commander deck only if every color in its color identity is also found in the color identity of the deck’s commander.”
- [**903.9b**](https://yawgatog.com/resources/magic-rules/#R9039b) “If a commander would be put into its owner’s hand or library from anywhere, its owner may put it into the command zone instead.”
- [**903.8**](https://yawgatog.com/resources/magic-rules/#R9038) “A commander cast from the command zone costs an additional {2} for each previous time the player casting it has cast it from the command zone that game.”

Confidence: High · CR 2026-08-19 · 37 s · 3 model calls · $0.18

## Out of scope: a price question

**Asked**

> how much is a snappy going for these days? thinking about picking up a playset

**Answered**

> I only answer Comprehensive Rules and Commander rules questions. Tournament policy (MTR/IPG) and questions that aren't about the rules are out of my scope: for policy, see the Magic Tournament Rules or ask a tournament judge.

(The run file records this outcome as `OutOfScope`. The sentence above is what Discord
and the web page show for it.)

Declined after the classification call, before synthesis · 3 s · 1 model call · $0.005
