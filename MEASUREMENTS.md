# What the layered encoding costs a search

Measured before the 8-bit encoding of [SPEC.md](SPEC.md) section 5 was made
the default, on vectors `ffrwd/describe` produced from real video:

| space | model | dims | vectors | from |
| --- | --- | --- | --- | --- |
| clip | X-CLIP, video tower | 512 | 29,150 | 16 files, 9.0 hours |
| text | all-MiniLM-L6-v2 over sound labels and transcripts | 384 | 2,735 | 5 files, 1.6 hours |

Queries were about a hundred text prompts per space, embedded with the model
that embeds a query into that space, and every stored vector used as a query
against the rest. The truth is a brute-force search over the original f32
vectors. Recall counts ties as hits.

## How close a reconstruction is

Cosine between a vector and what a reader rebuilds from planes 0 to K, mean
over the stored vectors, with no escapes:

| space | K = 0 | K = 3 | K = 7 | f16 |
| --- | --- | --- | --- | --- |
| clip | 0.679 | 0.915 | 0.9996 | 1.0000 |
| text | 0.786 | 0.991 | 0.99997 | 1.0000 |

The clip space is worse at every K for one reason: component 442 of an X-CLIP
video vector is the largest in 99.9% of vectors, is always positive, and holds
30% of the vector's energy. One scale per vector is that component's scale, and
4.8% of the other components quantize to zero with all eight planes. A MiniLM
vector's largest component holds 3.6%.

## Escapes

Sending the largest components exactly (section 5) fixes it. Clip space, text
prompts as queries, all eight planes:

| encoding | bytes per vector | recall@1 | recall@10 | recall@100 | threshold decisions flipped |
| --- | --- | --- | --- | --- | --- |
| I8, no escapes | 516 | 0.800 | 0.904 | 0.932 | 8.05% |
| I8, 1 escape | 520 | 0.982 | 0.980 | 0.988 | 1.51% |
| I8, 2 escapes | 524 | 0.991 | 0.988 | 0.991 | 1.07% |
| I8, 8 escapes | 548 | 0.991 | 0.991 | 0.994 | 0.74% |
| F16 | 1024 | 0.991 | 0.995 | 0.999 | |
| F32 | 2048 | 1 | 1 | 1 | 0 |

The last column is the share of vectors scoring above a cosine threshold of
0.25 against a prompt whose above-or-below answer changed. The ones that change
sit within about 0.001 of the threshold.

With one escape the clip space at K = 3 reaches cosine 0.992, which is what the
text space has with none. The text space gains nothing from escapes: its
recall@1 is 1.000 and its recall@100 is 0.998 without them.

Two escapes is the writer's default here. Choosing them per record costs 2
bytes more per escape than fixing the indices for a whole space would, and in
exchange a writer needs to know nothing about the model whose vectors it is
handed.

## Fewer planes

Text prompts as queries, no escapes:

| space | planes | recall@1 | recall@100 |
| --- | --- | --- | --- |
| text | 0 to 3 | 0.958 | 0.962 |
| text | 0 to 7 | 1.000 | 0.998 |
| clip | 0 to 3 | 0.146 | 0.363 |
| clip | 0 to 7 | 0.800 | 0.933 |

A coarse reader of a MiniLM space can stop at four planes. A reader of an
X-CLIP space without escapes cannot stop early at all.

## Searching plane 0 first

Hamming distance over the sign planes to pick N candidates, then a rescore of
those with all eight planes, recall@100:

| space | query | N = 1000 | N = 2000 |
| --- | --- | --- | --- |
| text | a stored vector | 0.987 | |
| text | a text prompt | 0.978 | |
| text | a text prompt, f32 against the signs | 0.996 | |
| clip | a stored vector | 0.939 | |
| clip | a text prompt | | 0.61 |
| clip | a text prompt, f32 against the signs | | 0.70 |

The sign plane is a good first stage everywhere except a text prompt against
X-CLIP video vectors, where it alone finds 15% of the true top hundred and no
rescoring brings back what was never gathered. Escapes do not change this: the
dominant component's sign is the same in every vector, so its bit never carried
anything. A searcher that holds the query at full precision should score it
against the sign bits directly, which beat Hamming distance at every candidate
count in both spaces.

## Two things a reader should know

- With few planes, compare by cosine or Hamming distance. A reconstruction from
  plane 0 is plus or minus `64/127 * scale`, the scale differs per record, and a
  bare dot product ranks partly by it.
- Rounding the scale to the nearest binary16 value would never clamp either
  (nearest is at worst 0.049% low, clamping needs 0.39%). The spec rounds up
  anyway, so that nobody has to take that on trust.
