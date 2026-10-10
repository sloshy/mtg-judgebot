---
title: Sample answers
description: What the judge's answers look like, as screenshots of the web app and five answers copied verbatim from a published evaluation run.
sidebar:
  order: 3
---

The following are sample answers given by the bot so you can see how it works without running it yourself.

![The web app answering a question about Dark Confidant and Tarmogoyf: the ruling, six linked citations, the cards the question was resolved to, and the confidence.](../../../assets/screenshots/web-answer-light.png)

A nickname that could mean several cards gets a question back instead of a guess:

![The web app asking which Tibalt was meant, with three cards to choose from.](../../../assets/screenshots/web-did-you-mean.png)

On Discord the answer arrives as a message, with the citations in an embed and the rating
buttons beneath it. The "did you mean…?" choices are buttons too.

![A Discord reply to /judge. It restates the question about revealing an X-cost spell to Dark Confidant and answers in two paragraphs that X counts as 0, with a rule number inline and mana symbols drawn as pictures. An embed quotes a dated Dark Confidant ruling, the card's Oracle text and rule 107.3g, and ends with "Cards: Dark Confidant", the confidence and the CR version. Three buttons follow: Incorrect, Partially correct, Correct.](../../../assets/screenshots/discord-answer-citations.png)

![A Discord reply asking which Tibalt was meant: a numbered list of three cards, and a button for each.](../../../assets/screenshots/discord-did-you-mean.png)

## Where these come from

The five answers below are copied from `eval/published/v1-opus-5-5.json`, the 2026-09-29
run of the [gold evaluation set](../../how-it-works/evaluation/) on the default
configuration (`claude-opus-5-5` on both stages, synthesis at medium effort, Voyage
embeddings). The run file holds all 21.

Nothing is edited except the one note on the last sample. Each question is as the gold
set asks it, typos and all. The answer, citations, time and cost are as the run recorded
them.

Every quote was checked verbatim against its source before the answer was accepted, and
each is shown whole. Rule numbers link to the Yawgatog mirror, as they do in a real
answer.

## Layers: Humility and Opalescence

**Asked**

> ok classic one. I have [[Humility]] out and my opponent drops [[Opalescence]]. what are Humility and Opalescence now?? are all my creatures still 1/1? does the answer change if Opalescence was out first?

**Answered**

> Your regular creatures stay 1/1 with no abilities either way. Only the size of **Humility** depends on which enchantment came first. **Opalescence** stays a noncreature enchantment with its ability, because it says "Each other non-Aura enchantment", so it never animates itself and Humility can't touch it.
>
> **Humility first, then Opalescence (your case):**
> - Layer 4: Opalescence turns Humility into an enchantment creature.
> - Layer 6: Humility removes all abilities from creatures, including its own. Per `613.6`, an effect that has started to apply keeps applying in the later layers "even if the ability generating the effect is removed during this process."
> - Layer 7b: both effects set base P/T, so they apply in timestamp order (`613.4b`, `613.7`). Humility's 1/1 applies first, then Opalescence sets each other non-Aura enchantment to its mana value. Humility ends up a 4/4 with no abilities, and your other creatures are 1/1.
>
> **Opalescence first, then Humility:** layers 4 and 6 work the same way. In 7b, Opalescence's effect applies first and Humility's later "1/1" overrides it, so Humility is a 1/1 with no abilities too.
>
> The Scryfall ruling on the pair walks through exactly this: Humility becomes 4/4 if it entered first, and 1/1 if Opalescence entered first. Any other non-Aura enchantments on the battlefield work like Humility does: they're mana-value-sized if Humility is older, 1/1 if Humility is newer, and they lose their abilities in both cases.

- **Ruling — Humility** “The type-changing effect applies at layer 4, but the rest happens in the applicable layers. The rest of it will apply even if the permanent loses its ability before it's finished applying.”
- [**613.6**](https://yawgatog.com/resources/magic-rules/#R6136) “If an effect starts to apply in one layer and/or sublayer, it will continue to be applied to the same set of objects in each other applicable layer and/or sublayer, even if the ability generating the effect is removed during this process.”
- [**613.4b**](https://yawgatog.com/resources/magic-rules/#R6134b) “Layer 7b: Effects that set power and/or toughness to a specific number or value are applied. Effects that refer to the base power and/or toughness of a creature apply in this layer.”
- [**613.7**](https://yawgatog.com/resources/magic-rules/#R6137) “An effect with an earlier timestamp is applied before an effect with a later timestamp.”
- **Oracle text — Opalescence** “Each other non-Aura enchantment is a creature in addition to its other types and has base power and base toughness each equal to its mana value.”

Confidence: High · CR 2026-08-19 · 21 s · 2 model calls · $0.10

## Trample and deathtouch, and a misremembered card

**Asked**

> I attack with a 5/5 trample that also has deathtouch (Questing Beast) and they block with a 4/4. how much can I trample over? my opponent says I have to assign 4 to the blocker first

**Answered**

> With deathtouch and trample you only need to assign **1** damage to the 4/4. The other 4 can trample over to the player (or planeswalker) you're attacking. Your opponent is wrong.
>
> Per `702.19b`, a trampler has to assign lethal damage to its blockers before any excess can go to the player, and `702.2c` says "Any nonzero amount of combat damage assigned to a creature by a source with deathtouch is considered to be lethal damage for the purposes of determining if excess damage is being dealt." So 1 damage counts as lethal to the 4/4, and the rest is excess you can assign however you like between the blocker and the player. The 1 point still destroys the 4/4 thanks to deathtouch (`702.2b`).
>
> One correction: **Questing Beast**'s current Oracle text doesn't include trample. Its keywords are only "Vigilance, deathtouch, haste". Without trample from some other source, a blocked Beast deals all its damage to the blocker and none to the player (`510.1c`). If it does have trample, its other line "Combat damage that would be dealt by creatures you control can't be prevented" also applies. Any combat damage the Beast deals to the opponent triggers its last ability, which deals the same amount to a target planeswalker that player controls.

- [**702.2c**](https://yawgatog.com/resources/magic-rules/#R7022c) “Any nonzero amount of combat damage assigned to a creature by a source with deathtouch is considered to be lethal damage for the purposes of determining if excess damage is being dealt.”
- [**702.19b**](https://yawgatog.com/resources/magic-rules/#R70219b) “Once all those blocking creatures are assigned lethal damage, any excess damage is assigned as its controller chooses among those blocking creatures and the player, planeswalker, or battle the creature is attacking.”
- [**702.2b**](https://yawgatog.com/resources/magic-rules/#R7022b) “A creature with toughness greater than 0 that’s been dealt damage by a source with deathtouch since the last time state-based actions were checked is destroyed as a state-based action.”
- **Oracle text — Questing Beast** “Vigilance, deathtouch, haste”
- [**510.1c**](https://yawgatog.com/resources/magic-rules/#R5101c) “If exactly one creature is blocking it, it assigns all its combat damage to that creature.”

Confidence: High · CR 2026-08-19 · 17 s · 3 model calls · $0.10

## An old printing against the current Oracle text

**Asked**

> old school question. my Waylay (Urza's Saga printing) says 'at end of turn, remove them from the game'. so I cast it during my opponent's end step, the knights survive since the end step already started, and I get to swing with them on my turn right? classic waylay trick

**Answered**

> No, that trick doesn't work anymore. **Waylay** has Oracle errata. Its current text reads "Create three 2/2 white Knight creature tokens. Exile them at the beginning of the next cleanup step." Cards printed with "at end of turn" were errata'd (`513.1a`), and Waylay's exile now happens in the cleanup step, which shuts down this exact play.
>
> The cleanup step comes straight after the end step in the same turn: "The ending phase consists of two steps: end and cleanup" (`512.1`). So if you cast Waylay during your opponent's end step, the next cleanup step is that same turn's cleanup. When it starts, the game checks whether "any triggered abilities are waiting to be put onto the stack (including those that trigger “at the beginning of the next cleanup step”)". If so, the trigger goes on the stack, players get priority, the Knights are exiled, and then another cleanup step begins (`514.3a`).
>
> The Knights only last for the rest of your opponent's turn. You can use them as surprise blockers if you cast Waylay earlier, or as sacrifice fodder, but they never make it to your turn.

- **Oracle text — Waylay** “Exile them at the beginning of the next cleanup step.”
- [**513.1a**](https://yawgatog.com/resources/magic-rules/#R5131a) “Cards that were printed with that text have received errata in the Oracle card reference”
- [**512.1**](https://yawgatog.com/resources/magic-rules/#R5121) “The ending phase consists of two steps: end and cleanup.”
- [**514.3a**](https://yawgatog.com/resources/magic-rules/#R5143a) “At this point, the game checks to see if any state-based actions would be performed and/or any triggered abilities are waiting to be put onto the stack (including those that trigger “at the beginning of the next cleanup step”).”

Confidence: High · CR 2026-08-19 · 13 s · 2 model calls · $0.08

## Commander: colour identity and the command zone

**Asked**

> commander q: my commander is [[Kenrith, the Returned King]]. two things: 1) can I run Nicol Bolas, the Ravager since Kenrith's text mentions all 5 colors? and 2) someone bounced Kenrith to my hand with Unsummon - does he go to the command zone or is that only for graveyard/exile?

**Answered**

> 1) Yes. **Kenrith, the Returned King** has all five colors in his color identity, so **Nicol Bolas, the Ravager** is legal in your deck. 2) If Kenrith is returned to your hand, you may put him into the command zone instead. That option isn't limited to the graveyard and exile.
>
> **Color identity:** Per `903.4`, a card's color identity includes "the color or colors of any mana symbols in that card’s mana cost or rules text". Kenrith costs {4}{W}, and his abilities use {R}, {G}, {W}, {U} and {B}, so his identity is WUBRG. Bolas's identity is blue, black and red. That comes from the {1}{U}{B}{R} front and the back face, which counts per `903.4d`. `903.5c` only requires that every color in a card's identity also appears in your commander's identity, and it does.
>
> **Unsummon:** Hand and library are covered by a separate rule from graveyard and exile. `903.9b` says: "If a commander would be put into its owner’s hand or library from anywhere, its owner may put it into the command zone instead." This is a replacement, so you decide as **Unsummon** resolves. You can let Kenrith go to your hand and recast him from there without commander tax, or send him to the command zone. The graveyard/exile version in `903.9a` is instead a state-based action that you choose after he gets there. Tax only counts casts from the command zone (`903.8`).

- [**903.4**](https://yawgatog.com/resources/magic-rules/#R9034) “The color identity of a card is the color or colors of any mana symbols in that card’s mana cost or rules text”
- [**903.4d**](https://yawgatog.com/resources/magic-rules/#R9034d) “The back face of a double-faced card (see rule 712) is included when determining a card’s color identity.”
- [**903.5c**](https://yawgatog.com/resources/magic-rules/#R9035c) “A card can be included in a Commander deck only if every color in its color identity is also found in the color identity of the deck’s commander.”
- [**903.9b**](https://yawgatog.com/resources/magic-rules/#R9039b) “If a commander would be put into its owner’s hand or library from anywhere, its owner may put it into the command zone instead.”
- [**903.9a**](https://yawgatog.com/resources/magic-rules/#R9039a) “If a commander is in a graveyard or in exile and that object was put into that zone since the last time state-based actions were checked, its owner may put it into the command zone.”
- [**903.8**](https://yawgatog.com/resources/magic-rules/#R9038) “A commander cast from the command zone costs an additional {2} for each previous time the player casting it has cast it from the command zone that game.”

Confidence: High · CR 2026-08-19 · 14 s · 2 model calls · $0.09

## Out of scope: a price question

**Asked**

> how much is a snappy going for these days? thinking about picking up a playset

**Answered**

> I only answer Comprehensive Rules and Commander rules questions. Tournament policy (MTR/IPG) and questions that aren't about the rules are out of my scope: for policy, see the Magic Tournament Rules or ask a tournament judge.

(The run file records this outcome as `OutOfScope`. The sentence above is what Discord
and the web app show for it.)

Declined after the classification call, before synthesis · 2 s · 1 model call · $0.002
