"""German speller word list + frequencies for nlprule's FST (suggestions and
compound parts): every form of LT's de_DE.dict (the morfologik speller
dictionary LT draws suggestions from, with its 0..25 frequency letters),
plus the stems of LT's spelling.txt and the plain words of ignore.txt.
usage: mkspeller.py <de_DE.raw from FSADecompile> <lt hunspell dir> <out filters dir>"""
import sys
raw, hdir, out = sys.argv[1:4]
freq = {}
for l in open(raw, encoding='utf-8'):
    w, _, f = l.rstrip('\n').rpartition('+')
    if not w or w.startswith('-') or w.endswith('-') or len(f) != 1:
        continue
    freq[w] = max(freq.get(w, 0), ord(f) - ord('A'))
extra = 0
for name in ['spelling.txt', 'spelling_custom.txt', 'ignore.txt']:
    for l in open(f'{hdir}/{name}', encoding='utf-8'):
        w = l.split('#')[0].strip().split('/')[0]
        if w and '*' not in w and ' ' not in w and w not in freq:
            freq[w] = 0
            extra += 1
with open(f'{out}/speller.txt', 'w', encoding='utf-8') as s, open(f'{out}/freq.txt', 'w', encoding='utf-8') as fq:
    for w in sorted(freq):
        s.write(w + '\n')
        if freq[w]:
            fq.write(f'{w}\t{freq[w]}\n')
print(len(freq), 'forms,', extra, 'from LT lists')
