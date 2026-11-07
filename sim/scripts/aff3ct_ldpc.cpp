// Cairn LDPC reference harness built on libaff3ct decoder modules.
// Needed because AFF3CT's simulator cannot puncture/erase LDPC bits.
// All-zero codeword over BPSK/AWGN (valid: linear code, symmetric decoders).
// LLR = 2y/sigma^2 on sent positions, 0 on punctured / unsent (erasures).
// usage: cairn_ldpc ALIST META TYPE IMPLEM ITER EBN0_DB MIN_ERR MAX_FRAMES SEED [NMS_FACTOR]
//   TYPE: FLOOD | LAYERED     IMPLEM: SPA | NMS | MS
#include <aff3ct.hpp>
#include <cmath>
#include <cstdlib>
#include <cstdio>
#include <fstream>
#include <random>
#include <string>
#include <vector>
using namespace aff3ct;

template<class Dec>
static void run(Dec& dec, int N, int K, const std::vector<int>& sent, double R, double ebn0,
                long min_err, long max_frames, unsigned seed)
{
    std::vector<float> llr(N);
    std::vector<int> cw(N);
    if (const char* cwf = std::getenv("CW_FILE"))
    {   // round trip: noiseless +-8 LLRs from the sim's own codewords, erasures at LLR 0
        std::ifstream f(cwf);
        std::string line;
        int words = 0, ok = 0;
        while (std::getline(f, line))
        {
            if ((int)line.size() != N) continue;
            for (int i = 0; i < N; i++)
                llr[i] = sent[i] ? (line[i] == '1' ? -8.f : 8.f) : 0.f;
            dec.decode_siho_cw(llr.data(), cw.data(), -1, false);
            bool same = true;
            for (int i = 0; i < N; i++) if (cw[i] != (line[i] == '1')) same = false;
            words++; ok += same;
        }
        std::printf("roundtrip %d/%d codewords recovered exactly\n", ok, words);
        return;
    }
    double sigma2 = 1.0 / (2.0 * R * std::pow(10.0, ebn0 / 10.0));
    double sigma = std::sqrt(sigma2);
    std::mt19937_64 rng(seed);
    std::normal_distribution<double> g(0.0, 1.0);
    long err = 0, fr = 0;
    while (err < min_err && fr < max_frames)
    {
        for (int i = 0; i < N; i++)
            llr[i] = sent[i] ? (float)(2.0 * (1.0 + sigma * g(rng)) / sigma2) : 0.f;
        dec.decode_siho_cw(llr.data(), cw.data(), -1, false);
        bool bad = false;
        for (int i = 0; i < K; i++)
            if (cw[i]) { bad = true; break; }
        err += bad;
        fr++;
    }
    std::printf("%.4f %ld %ld %.6g\n", ebn0, err, fr, (double)err / fr);
}

int main(int argc, char** argv)
{
    if (argc < 10) { std::fprintf(stderr, "bad args\n"); return 2; }
    std::string alist = argv[1], meta = argv[2], type = argv[3], impl = argv[4];
    int iters = std::atoi(argv[5]);
    double ebn0 = std::atof(argv[6]);
    long min_err = std::atol(argv[7]), max_frames = std::atol(argv[8]);
    unsigned seed = (unsigned)std::atol(argv[9]);
    float nf = argc > 10 ? (float)std::atof(argv[10]) : 0.75f;

    std::ifstream mf(meta);
    int N, M, K, P, E;
    mf >> N >> M >> K >> P >> E;
    std::vector<int> sent(N);
    for (int i = 0; i < N; i++) mf >> sent[i];
    double R = (double)K / E;

    auto H = tools::LDPC_matrix_handler::read(alist);
    if ((int)H.get_n_rows() != N || (int)H.get_n_cols() != M) { std::fprintf(stderr, "H size mismatch\n"); return 3; }
    std::vector<unsigned> info(K);
    for (int i = 0; i < K; i++) info[i] = i;
    unsigned dmax = (unsigned)H.get_cols_max_degree();

#define GO(CLS, RULE)                                                                           \
    {                                                                                           \
        module::CLS<int, float, tools::RULE<float>> d(K, N, iters, H, info, tools::RULE<float>ARGS, true, 1); \
        run(d, N, K, sent, R, ebn0, min_err, max_frames, seed);                                 \
        return 0;                                                                               \
    }
    if (type == "FLOOD")
    {
        if (impl == "SPA") {
#define ARGS (dmax)
            GO(Decoder_LDPC_BP_flooding, Update_rule_SPA)
#undef ARGS
        }
        if (impl == "NMS") {
#define ARGS (nf)
            GO(Decoder_LDPC_BP_flooding, Update_rule_NMS)
#undef ARGS
        }
    }
    else if (type == "LAYERED")
    {
        if (impl == "SPA") {
#define ARGS (dmax)
            GO(Decoder_LDPC_BP_horizontal_layered, Update_rule_SPA)
#undef ARGS
        }
        if (impl == "NMS") {
#define ARGS (nf)
            GO(Decoder_LDPC_BP_horizontal_layered, Update_rule_NMS)
#undef ARGS
        }
    }
    std::fprintf(stderr, "unsupported combo\n");
    return 2;
}
