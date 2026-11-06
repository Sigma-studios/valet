# Spelling and grammar data

`<lang>_tokenizer.bin.zst` and `<lang>_rules.bin.zst` are nlprule binaries
(zstd -19) compiled from LanguageTool 6.5, embedded in valet by `src/spell.rs`.
They are derived from LanguageTool and so under the LGPL 2.1, like
LanguageTool's data; the code in `vendor/nlprule` is MIT or Apache-2.0.

`vendor/nlprule` is nlprule's pull request #92 (commit `19f80ac`, "all 35
LanguageTool languages") with valet's fixes: LanguageTool's French word
tokenizer, sentence-relative offsets for the built-in rules, sentence ends after
whitespace, English `n't` and dashes, the speller's accent search and ranking,
disambiguation filters that match nothing, inflected matching, and the French
and English word-repeat exceptions. Rebuilding the data needs the pull
request's build tools, which are not vendored:

```sh
git clone https://github.com/bminixhofer/nlprule && cd nlprule
git fetch origin pull/92/head:pr92 && git checkout 19f80ac
rsync -a --exclude bin <valet>/vendor/nlprule/src/ nlprule/src/   # valet's fixes
curl -LO https://languagetool.org/download/LanguageTool-6.5.zip && unzip LanguageTool-6.5.zip
python3 -m venv venv && venv/bin/pip install lxml wordfreq chardet   # Java 11+ on PATH too
(cd build && ../venv/bin/python make_build_dirs.py --lt_dir ../LanguageTool-6.5 --out_root ../data2 --langs en fr de)
for l in en fr; do venv/bin/python <valet>/spell/mkfreq.py $l data2/$l/filters; done   # suggestion ranking
cargo build --release --features "compile,bin,zh,ja"
for l in en fr de; do
  target/release/compile --build-dir data2/$l --tokenizer-out ${l}_tokenizer.bin --rules-out ${l}_rules.bin
  zstd -19 ${l}_tokenizer.bin -o <valet>/spell/${l}_tokenizer.bin.zst
  zstd -19 ${l}_rules.bin -o <valet>/spell/${l}_rules.bin.zst
done
```

German's speller also needs LanguageTool's hunspell files and a word list
dumped from its `de_DE.dict` (with frequencies) in `data2/de/filters/` before
compiling: `filters/hunspell/` holds `de_DE.aff`, `de_DE.dic`, `ignore.txt`,
`spelling.txt` and `prohibit.txt` from
`LanguageTool-6.5/org/languagetool/resource/de/hunspell/`, and `speller.txt` and
`freq.txt` come from `spell/mkspeller_de.py`:

```sh
H=LanguageTool-6.5/org/languagetool/resource/de/hunspell
mkdir -p data2/de/filters/hunspell && cp $H/de_DE.aff $H/de_DE.dic $H/ignore.txt $H/spelling.txt $H/prohibit.txt data2/de/filters/hunspell/
java -cp "LanguageTool-6.5/libs/*" morfologik.tools.FSADecompile -i $H/de_DE.dict -o de_DE.raw
python3 <valet>/spell/mkspeller_de.py de_DE.raw $H data2/de/filters
```

Compiling takes a few minutes and about 1 GB per language.
