# Sweep efforts

22 questions, up to 2 repeats per cell; grader claude-opus-5-5; 4 questions at once in a run and 3 runs at once, so secs/q includes waiting on the others. Each figure is the mean over the cell's complete runs, with the range across runs in brackets. Cost is per run of all questions.

## Extraction

source = source matches gold; cards = resolved cards equal gold; primary = primary category among gold's; shown = gold rule ids the synthesis prompt would show.

| cell | runs | source | cards | primary | shown | errors | cost | secs/q |
|---|---|---|---|---|---|---|---|---|
| claude-opus-5-5@low | 2 | 100% (100%–100%) | 98% (95%–100%) | 100% (100%–100%) | 80% (77%–83%) | 0.0 (0.0–0.0) | $0.104 ($0.092–$0.115) | 3.0s (2.7s–3.2s) |
| claude-opus-5-5@medium | 2 | 100% (100%–100%) | 100% (100%–100%) | 100% (100%–100%) | 81% (80%–81%) | 0.0 (0.0–0.0) | $0.135 ($0.134–$0.135) | 3.4s (3.3s–3.5s) |
| claude-opus-5-5@high | 2 | 100% (100%–100%) | 100% (100%–100%) | 100% (100%–100%) | 81% (80%–81%) | 0.0 (0.0–0.0) | $0.182 ($0.175–$0.188) | 4.5s (4.4s–4.6s) |
| claude-opus-5-5@xhigh | 2 | 100% (100%–100%) | 100% (100%–100%) | 100% (100%–100%) | 82% (81%–83%) | 0.0 (0.0–0.0) | $0.264 ($0.262–$0.267) | 5.9s (5.8s–6.0s) |
| claude-sonnet-5-5@low | 2 | 100% (100%–100%) | 93% (91%–95%) | 95% (95%–95%) | 77% (74%–80%) | 0.0 (0.0–0.0) | $0.065 ($0.059–$0.070) | 2.2s (2.1s–2.3s) |
| claude-sonnet-5-5@medium | 2 | 100% (100%–100%) | 91% (91%–91%) | 97% (95%–100%) | 74% (71%–76%) | 0.0 (0.0–0.0) | $0.059 ($0.059–$0.059) | 2.1s (2.1s–2.1s) |
| claude-sonnet-5-5@high | 2 | 100% (100%–100%) | 95% (95%–95%) | 100% (100%–100%) | 75% (74%–76%) | 0.0 (0.0–0.0) | $0.087 ($0.085–$0.089) | 2.9s (2.8s–3.1s) |
| claude-sonnet-5-5@xhigh | 2 | 100% (100%–100%) | 100% (100%–100%) | 100% (100%–100%) | 81% (80%–81%) | 0.0 (0.0–0.0) | $0.152 ($0.151–$0.152) | 4.8s (4.8s–4.8s) |
| claude-haiku-5-5@low | 2 | 100% (100%–100%) | 86% (86%–86%) | 100% (100%–100%) | 73% (70%–76%) | 0.0 (0.0–0.0) | $0.005 ($0.005–$0.005) | 2.5s (2.4s–2.5s) |
| claude-haiku-5-5@medium | 2 | 100% (100%–100%) | 84% (82%–86%) | 100% (100%–100%) | 65% (63%–67%) | 0.0 (0.0–0.0) | $0.008 ($0.008–$0.008) | 3.7s (3.6s–3.8s) |
| claude-haiku-5-5@high | 2 | 100% (100%–100%) | 95% (95%–95%) | 100% (100%–100%) | 71% (67%–74%) | 0.0 (0.0–0.0) | $0.011 ($0.011–$0.011) | 4.7s (4.7s–4.8s) |
| claude-haiku-5-5@xhigh | 2 | 84% (82%–86%) | 77% (77%–77%) | 100% (100%–100%) | 52% (43%–61%) | 3.5 (3.0–4.0) | $0.016 ($0.015–$0.016) | 6.4s (6.3s–6.5s) |

## Synthesis

shape = answered or declined as gold says; errors = questions that ended in an error where gold expects an answer; recall = decisive rule ids cited; retried = first attempt rejected; graded = answers graded / questions gold expects an answer to; agree, partial and follows are shares of those questions (an error or an ungraded answer counts against them), then the count pooled over the runs: on 19 questions one answer is about 5 points of a run, so a gap of an answer or two is noise; supported = claims the quotes support, over graded answers; cold = questions whose material the run wrote to the cache itself rather than reading another run's copy, as every production question does (`-` in run files from before it was recorded); cost is only comparable between cold runs.

| cell | runs | shape | errors | recall | retried | graded | agree | partial | follows | supported | cold | cost | grading | secs/q |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| extract=gold synth=claude-opus-5-5@low | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 84% (81%–86%) | 0.0 (0.0–0.0) | 19/19, 19/19 | 100% (100%–100%) · 38/38 | 0% (0%–0%) · 0/38 | 50% (42%–58%) · 19/38 | 63% (62%–65%) | -, - | $1.411 ($1.391–$1.431) | $0.430 ($0.419–$0.440) | 7.5s (6.5s–8.5s) |
| extract=gold synth=claude-opus-5-5@medium | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 89% (86%–92%) | 0.0 (0.0–0.0) | 19/19, 19/19 | 100% (100%–100%) · 38/38 | 0% (0%–0%) · 0/38 | 55% (53%–58%) · 21/38 | 69% (69%–69%) | -, - | $1.507 ($1.506–$1.508) | $0.433 ($0.433–$0.434) | 8.5s (8.4s–8.6s) |
| extract=gold synth=claude-opus-5-5@high | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 91% (89%–92%) | 0.0 (0.0–0.0) | 19/19, 19/19 | 100% (100%–100%) · 38/38 | 0% (0%–0%) · 0/38 | 42% (37%–47%) · 16/38 | 66% (65%–66%) | -, - | $1.564 ($1.557–$1.571) | $0.425 ($0.413–$0.437) | 9.3s (9.3s–9.4s) |
| extract=gold synth=claude-opus-5-5@xhigh | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 93% (92%–95%) | 0.0 (0.0–0.0) | 19/19, 19/19 | 100% (100%–100%) · 38/38 | 0% (0%–0%) · 0/38 | 53% (37%–68%) · 20/38 | 66% (62%–70%) | -, - | $1.766 ($1.759–$1.774) | $0.462 ($0.433–$0.490) | 13.2s (13.1s–13.3s) |
| extract=gold synth=claude-sonnet-5-5@low | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 81% (81%–81%) | 3.5 (3.0–4.0) | 19/19, 19/19 | 97% (95%–100%) · 37/38 | 3% (0%–5%) · 1/38 | 34% (32%–37%) · 13/38 | 58% (57%–59%) | -, 19/19 | $0.751 ($0.741–$0.761) | $0.397 ($0.380–$0.413) | 5.3s (5.2s–5.4s) |
| extract=gold synth=claude-sonnet-5-5@medium | 2 | 95% (95%–95%) | 1.0 (1.0–1.0) | 81% (78%–84%) | 5.0 (5.0–5.0) | 18/19, 18/19 | 89% (89%–89%) · 34/38 | 5% (5%–5%) · 2/38 | 39% (37%–42%) · 15/38 | 62% (61%–63%) | 19/19, 19/19 | $0.831 ($0.821–$0.841) | $0.380 ($0.367–$0.392) | 5.8s (5.8s–5.9s) |
| extract=gold synth=claude-sonnet-5-5@high | 2 | 95% (95%–95%) | 1.0 (1.0–1.0) | 81% (78%–84%) | 6.5 (6.0–7.0) | 18/19, 18/19 | 95% (95%–95%) · 36/38 | 0% (0%–0%) · 0/38 | 50% (47%–53%) · 19/38 | 67% (65%–69%) | 19/19, - | $1.006 ($1.001–$1.011) | $0.361 ($0.349–$0.374) | 10.0s (9.7s–10.3s) |
| extract=gold synth=claude-sonnet-5-5@xhigh | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 89% (84%–95%) | 1.5 (0.0–3.0) | 19/19, 19/19 | 97% (95%–100%) · 37/38 | 3% (0%–5%) · 1/38 | 37% (37%–37%) · 14/38 | 66% (66%–66%) | -, - | $1.024 ($0.965–$1.083) | $0.429 ($0.415–$0.443) | 12.2s (11.0s–13.5s) |
| extract=gold synth=claude-haiku-5-5@low | 2 | 98% (95%–100%) | 0.5 (0.0–1.0) | 69% (68%–70%) | 4.0 (4.0–4.0) | 18/19, 19/19 | 76% (68%–84%) · 29/38 | 16% (16%–16%) · 6/38 | 39% (37%–42%) · 15/38 | 67% (63%–70%) | -, 19/19 | $0.043 ($0.043–$0.043) | $0.348 ($0.322–$0.375) | 6.2s (6.1s–6.3s) |
| extract=gold synth=claude-haiku-5-5@medium | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 77% (76%–78%) | 2.5 (2.0–3.0) | 19/19, 19/19 | 87% (84%–89%) · 33/38 | 13% (11%–16%) · 5/38 | 42% (32%–53%) · 16/38 | 69% (68%–71%) | 19/19, - | $0.051 ($0.051–$0.051) | $0.373 ($0.370–$0.377) | 9.2s (9.1s–9.3s) |
| extract=gold synth=claude-haiku-5-5@high | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 80% (78%–81%) | 0.5 (0.0–1.0) | 19/19, 19/19 | 89% (89%–89%) · 34/38 | 11% (11%–11%) · 4/38 | 55% (53%–58%) · 21/38 | 74% (72%–76%) | -, - | $0.059 ($0.059–$0.059) | $0.382 ($0.376–$0.387) | 12.2s (12.0s–12.3s) |
| extract=gold synth=claude-haiku-5-5@xhigh | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 82% (81%–84%) | 0.0 (0.0–0.0) | 19/19, 19/19 | 89% (89%–89%) · 34/38 | 11% (11%–11%) · 4/38 | 58% (58%–58%) · 22/38 | 75% (74%–77%) | -, 19/19 | $0.089 ($0.088–$0.091) | $0.396 ($0.376–$0.415) | 22.4s (22.3s–22.5s) |

## End to end

shape = answered or declined as gold says; errors = questions that ended in an error where gold expects an answer; recall = decisive rule ids cited; retried = first attempt rejected; graded = answers graded / questions gold expects an answer to; agree, partial and follows are shares of those questions (an error or an ungraded answer counts against them), then the count pooled over the runs: on 19 questions one answer is about 5 points of a run, so a gap of an answer or two is noise; supported = claims the quotes support, over graded answers; cold = questions whose material the run wrote to the cache itself rather than reading another run's copy, as every production question does (`-` in run files from before it was recorded); cost is only comparable between cold runs.

| cell | runs | shape | errors | recall | retried | graded | agree | partial | follows | supported | cold | cost | grading | secs/q |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| extract=claude-opus-5-5@low synth=claude-opus-5-5@medium | 2 | 100% (100%–100%) | 0.0 (0.0–0.0) | 84% (81%–86%) | 0.0 (0.0–0.0) | 19/19, 19/19 | 97% (95%–100%) · 37/38 | 3% (0%–5%) · 1/38 | 50% (47%–53%) · 19/38 | 68% (65%–71%) | 19/19, 19/19 | $1.641 ($1.599–$1.682) | $0.447 ($0.432–$0.462) | 11.2s (10.8s–11.7s) |
| extract=claude-sonnet-5-5@low synth=claude-sonnet-5-5@high | 2 | 95% (95%–95%) | 1.0 (1.0–1.0) | 77% (73%–81%) | 4.0 (3.0–5.0) | 18/19, 18/19 | 92% (89%–95%) · 35/38 | 3% (0%–5%) · 1/38 | 39% (37%–42%) · 15/38 | 61% (59%–63%) | 18/18, 18/18 | $0.955 ($0.937–$0.974) | $0.375 ($0.369–$0.381) | 12.0s (11.1s–12.9s) |

