#define COMPUTE_ROOT "RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED),RootConstants(num32BitConstants=2,b0),UAV(u0)"
cbuffer Bindings : register(b0) { uint source_slot; uint destination_slot; }
RWByteAddressBuffer arguments : register(u0);

[RootSignature(COMPUTE_ROOT)]
[numthreads(8, 4, 1)]
void build_hiz(uint3 id : SV_DispatchThreadID) {
    Texture2D<float> depth = ResourceDescriptorHeap[source_slot];
    RWTexture2D<float> hiz = ResourceDescriptorHeap[destination_slot];
    int2 p = int2(id.xy) * 2;
    float a = depth.Load(int3(p, 0));
    float b = depth.Load(int3(p + int2(1, 0), 0));
    float c = depth.Load(int3(p + int2(0, 1), 0));
    float d = depth.Load(int3(p + int2(1, 1), 0));
    hiz[id.xy] = max(max(a, b), max(c, d));
}

[RootSignature(COMPUTE_ROOT)]
[numthreads(1, 1, 1)]
void cull() {
    Texture2D<float> hiz = ResourceDescriptorHeap[source_slot];
    uint visible = hiz.Load(int3(0, 0, 0)) >= 0.5 ? 1 : 0;
    arguments.Store3(0, uint3(visible, 1, 1));
}

struct Vertex {
    float4 position : SV_Position;
};

[RootSignature("")]
[outputtopology("triangle")]
[numthreads(1, 1, 1)]
void mesh(out vertices Vertex vertices[3], out indices uint3 triangles[1]) {
    SetMeshOutputCounts(3, 1);
    vertices[0].position = float4(-0.8, -0.8, 0, 1);
    vertices[1].position = float4( 0.8, -0.8, 0, 1);
    vertices[2].position = float4( 0.0,  0.8, 0, 1);
    triangles[0] = uint3(0, 1, 2);
}

float4 pixel() : SV_Target {
    return float4(1, 0, 1, 1);
}
