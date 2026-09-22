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
21.

Nothing is edited except the one note on the last sample. Each question is as the gold
set asks it, typos and all. The answer, citations, time and cost are as the run recorded
them.

Every quote was checked verbatim against its source before the answer was accepted. The
run file cuts long quotes at a display limit, which is the "…" at the end of a few below.
Validation checked the full span. Rule numbers link to the Yawgatog mirror, as they do in
a real answer.

## Layers: Humility and Opalescence

**Asked**

> ok classic one. I have [[Humility]] out and my opponent drops [[Opalescence]]. what are Humility and Opalescence now?? are all my creatures still 1/1? does the answer change if Opalescence was out first?

**Answered**

> Your other creatures are still 1/1 with no abilities either way. **Opalescence** is never a creature, because it "Each other non-Aura enchantment" leaves Opalescence itself out. **Humility**'s size is the only thing that depends on timestamps.
>
> **Humility out first, Opalescence enters later (your case):**
> - Layer 4: Opalescence turns Humility into an enchantment creature.
> - Layer 6: Humility is now a creature, so it loses all abilities, including its own. Per `613.6`, an effect that has started applying keeps applying in its other layers "even if the ability generating the effect is removed during this process." So Humility's effect still applies in layer 7b.
> - Layer 7b: Both effects set base P/T, so they apply in timestamp order (`613.4b`, `613.7`). A permanent gets its timestamp when it enters (`613.7d`). Humility makes every creature 1/1 first. Then Opalescence makes Humility 4/4, because its mana value is 4.
>
> Result: Humility is a 4/4 enchantment creature with no abilities. Your other creatures are 1/1 with no abilities. Opalescence is a normal enchantment and keeps its ability.
>
> **Opalescence out first:** The layers work the same way, but in 7b Opalescence makes Humility 4/4 first. Then Humility's later effect makes it 1/1. So Humility is 1/1 with no abilities, and everything else is still 1/1. The official Scryfall ruling on these two cards walks through both orders the same way.
>
> Any other non-Aura enchantments on the battlefield also become creatures, and they lose their abilities too. Their size follows the same timestamp logic. If Humility was there first, they get P/T equal to their mana value. If Opalescence was there first, they're 1/1.

- [**613.6**](https://yawgatog.com/resources/magic-rules/#R6136) “If an effect starts to apply in one layer and/or sublayer, it will continue to be applied to the same set of objects in each other applicable layer and/or sublayer, even if the ability generating the …”
- [**613.4b**](https://yawgatog.com/resources/magic-rules/#R6134b) “Layer 7b: Effects that set power and/or toughness to a specific number or value are applied. Effects that refer to the base power and/or toughness of a creature apply in this layer.”
- [**613.7**](https://yawgatog.com/resources/magic-rules/#R6137) “An effect with an earlier timestamp is applied before an effect with a later timestamp.”
- [**613.7d**](https://yawgatog.com/resources/magic-rules/#R6137d) “An object receives a timestamp at the time it enters a zone.”
- **Ruling — Humility** “Layer 7b: Humility becomes 1/1 and Worship becomes 1/1 (Humility). Humility becomes 4/4 and Worship becomes 4/4 (Opalescence).”
- **Oracle text — Opalescence** “Each other non-Aura enchantment is a creature in addition to its other types and has base power and base toughness each equal to its mana value.”

Confidence: High · CR 2026-08-19 · 20 s · 2 model calls · $0.10

## Trample and deathtouch, and a misremembered card

**Asked**

> I attack with a 5/5 trample that also has deathtouch (Questing Beast) and they block with a 4/4. how much can I trample over? my opponent says I have to assign 4 to the blocker first

**Answered**

> You can assign just 1 to the 4/4 and trample 4 over, so your opponent is wrong. One thing first: **Questing Beast** doesn't have trample on its own. Its current Oracle text is "Vigilance, deathtouch, haste" plus its other abilities, and its base size is 4/4. So this answer assumes something is making it 5/5 and giving it trample.
>
> With trample, you have to assign lethal damage to the blockers before any excess can go to the player, per `702.19b`. Deathtouch changes what counts as lethal: per `702.2c`, "any nonzero amount of combat damage assigned to a creature by a source with deathtouch is considered to be lethal damage". So 1 damage is lethal to the 4/4, and the other 4 can go to the defending player (or the planeswalker or battle it's attacking). You can also put more on the blocker if you want. Damage already marked on the blocker doesn't change this.
>
> If the 4 trample damage hits an opponent, Questing Beast's last ability also triggers, dealing 4 damage to target planeswalker that player controls.

- [**702.19b**](https://yawgatog.com/resources/magic-rules/#R70219b) “Once all those blocking creatures are assigned lethal damage, any excess damage is assigned as its controller chooses among those blocking creatures and the player, planeswalker, or battle the creatur…”
- [**702.2c**](https://yawgatog.com/resources/magic-rules/#R7022c) “Any nonzero amount of combat damage assigned to a creature by a source with deathtouch is considered to be lethal damage for the purposes of determining if excess damage is being dealt.”
- **Oracle text — Questing Beast** “Vigilance, deathtouch, haste”
- **Oracle text — Questing Beast** “Whenever Questing Beast deals combat damage to an opponent, it deals that much damage to target planeswalker that player controls.”

Confidence: High · CR 2026-08-19 · 13 s · 3 model calls · $0.10

## An old printing against the current Oracle text

**Asked**

> old school question. my Waylay (Urza's Saga printing) says 'at end of turn, remove them from the game'. so I cast it during my opponent's end step, the knights survive since the end step already started, and I get to swing with them on my turn right? classic waylay trick

**Answered**

> No, that trick doesn't work anymore. **Waylay** has Oracle errata. Its current text reads "Create three 2/2 white Knight creature tokens. Exile them at the beginning of the next cleanup step." Cards printed with "at end of turn" got updated Oracle wording (`513.1a`), and Waylay's exile was moved to the cleanup step specifically to shut down this play.
>
> If you cast it during your opponent's end step, the next cleanup step is the very next step of that same turn: "The ending phase consists of two steps: end and cleanup" (`512.1`). When that cleanup step starts, the game checks for any triggered abilities waiting to go on the stack, "including those that trigger “at the beginning of the next cleanup step”". The exile trigger goes on the stack, players get priority, it resolves and the Knights are exiled. Then another cleanup step begins (`514.3a`).
>
> So the Knights only last for the rest of your opponent's end step. They can still work as surprise blockers if you cast Waylay earlier in combat, or as sacrifice fodder, but they never make it to your turn.

- **Oracle text — Waylay** “Create three 2/2 white Knight creature tokens. Exile them at the beginning of the next cleanup step.”
- [**513.1a**](https://yawgatog.com/resources/magic-rules/#R5131a) “Cards that were printed with that text have received errata in the Oracle card reference to say “at the beginning of the end step” or “at the beginning of the next end step.””
- [**512.1**](https://yawgatog.com/resources/magic-rules/#R5121) “The ending phase consists of two steps: end and cleanup.”
- [**514.3a**](https://yawgatog.com/resources/magic-rules/#R5143a) “At this point, the game checks to see if any state-based actions would be performed and/or any triggered abilities are waiting to be put onto the stack (including those that trigger “at the beginning …”

Confidence: High · CR 2026-08-19 · 12 s · 2 model calls · $0.08

## Commander: colour identity and the command zone

**Asked**

> commander q: my commander is [[Kenrith, the Returned King]]. two things: 1) can I run Nicol Bolas, the Ravager since Kenrith's text mentions all 5 colors? and 2) someone bounced Kenrith to my hand with Unsummon - does he go to the command zone or is that only for graveyard/exile?

**Answered**

> 1) Yes. 2) Your choice: you can put **Kenrith** into the command zone instead of your hand, or leave him in your hand. The command zone option isn't only for the graveyard or exile.
>
> **1)** Per `903.4`, color identity counts the mana symbols in a card's mana cost *and* its rules text. Kenrith costs {4}{W}, and his abilities use {R}, {G}, {W}, {U} and {B} (for example "{4}{B}: Put target creature card from a graveyard onto the battlefield under its owner's control."). So his color identity is all five colors. **Nicol Bolas, the Ravager** is blue, black and red. His back face counts too, per `903.4d`, but it's also blue, black and red. Every color in Bolas's identity is in Kenrith's, so `903.5c` lets you run him.
>
> **2)** **Unsummon** says "Return target creature to its owner's hand." Per `903.9b`, if your commander would go to your hand or library from anywhere, you may put it into the command zone instead. This is a replacement effect, so Kenrith never touches your hand. It's optional. If you keep him in hand, you just cast him from your hand like a normal card, and commander tax won't apply, because per `903.8` the tax only applies when you cast him from the command zone.

- [**903.4**](https://yawgatog.com/resources/magic-rules/#R9034) “The color identity of a card is the color or colors of any mana symbols in that card’s mana cost or rules text”
- [**903.4d**](https://yawgatog.com/resources/magic-rules/#R9034d) “The back face of a double-faced card (see rule 712) is included when determining a card’s color identity.”
- [**903.5c**](https://yawgatog.com/resources/magic-rules/#R9035c) “A card can be included in a Commander deck only if every color in its color identity is also found in the color identity of the deck’s commander.”
- [**903.9b**](https://yawgatog.com/resources/magic-rules/#R9039b) “If a commander would be put into its owner’s hand or library from anywhere, its owner may put it into the command zone instead.”
- [**903.8**](https://yawgatog.com/resources/magic-rules/#R9038) “A commander cast from the command zone costs an additional {2} for each previous time the player casting it has cast it from the command zone that game.”
- **Oracle text — Kenrith, the Returned King** “{4}{B}: Put target creature card from a graveyard onto the battlefield under its owner's control.”
- **Oracle text — Unsummon** “Return target creature to its owner's hand.”

Confidence: High · CR 2026-08-19 · 14 s · 2 model calls · $0.10

## Out of scope: a price question

**Asked**

> how much is a snappy going for these days? thinking about picking up a playset

**Answered**

> I only answer Comprehensive Rules and Commander rules questions. Tournament policy (MTR/IPG) and questions that aren't about the rules are out of my scope: for policy, see the Magic Tournament Rules or ask a tournament judge.

(The run file records this outcome as `OutOfScope`. The sentence above is what Discord
and the web page show for it.)

Declined after the classification call, before synthesis · 2 s · 1 model call · $0.002
