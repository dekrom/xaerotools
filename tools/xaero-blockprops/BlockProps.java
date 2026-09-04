// Dumps every Minecraft block state, in BLOCK_STATE_REGISTRY id order, with the
// properties Xaero's MapWriter column algorithm consults. Run once per MC
// version against a Mojang-remapped client jar (see fetch.sh + README.md); the
// result is baked into assets/blockprops.bin and shipped.
//
// The id order is the same global blockstate numbering the zvcr file format
// stores, so the table doubles as the zvcr id -> block state resolver.

import java.io.DataOutputStream;
import java.io.BufferedOutputStream;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import net.minecraft.SharedConstants;
import net.minecraft.core.BlockPos;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.server.Bootstrap;
import net.minecraft.tags.BlockTags;
import net.minecraft.world.level.EmptyBlockGetter;
import net.minecraft.world.level.block.AirBlock;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.Blocks;
import net.minecraft.world.level.block.DoublePlantBlock;
import net.minecraft.world.level.block.FlowerBlock;
import net.minecraft.world.level.block.HalfTransparentBlock;
import net.minecraft.world.level.block.LiquidBlock;
import net.minecraft.world.level.block.PitcherCropBlock;
import net.minecraft.world.level.block.RenderShape;
import net.minecraft.world.level.block.TallFlowerBlock;
import net.minecraft.world.level.block.TransparentBlock;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.block.state.properties.Property;
import net.minecraft.world.level.material.FluidState;
import net.minecraft.world.level.material.MapColor;
import net.minecraft.world.level.material.PushReaction;
import net.minecraft.client.renderer.ItemBlockRenderTypes;
import net.minecraft.client.renderer.RenderType;

public final class BlockProps {
    // Keep in sync with crates/xaero-zvcr/src/blockprops.rs.
    static final int F_AIR                = 1 << 0;
    static final int F_RENDER_INVISIBLE   = 1 << 1;
    static final int F_HAS_MAP_COLOR      = 1 << 2;
    static final int F_FLUID_WATER        = 1 << 3;
    static final int F_FLUID_LAVA         = 1 << 4;
    static final int F_LIQUID_BLOCK       = 1 << 5;
    static final int F_CAN_BE_REPLACED    = 1 << 6;
    static final int F_IGNITED_BY_LAVA    = 1 << 7;
    static final int F_PUSH_DESTROY       = 1 << 8;
    static final int F_TRANSLUCENT_LAYER  = 1 << 9;
    static final int F_SHAPE_FULL_BLOCK   = 1 << 10;
    static final int F_TRANSPARENT_CLASS  = 1 << 11; // instanceof TransparentBlock
    static final int F_HALF_TRANSP_CLASS  = 1 << 12; // instanceof HalfTransparentBlock
    static final int F_DOUBLE_PLANT       = 1 << 13;
    static final int F_FLOWERISH          = 1 << 14;
    static final int F_GRASS_BLOCK        = 1 << 15;
    static final int F_WATER_BLOCK        = 1 << 16;
    static final int F_TORCH              = 1 << 17;
    static final int F_SHORT_GRASS        = 1 << 18;
    static final int F_GLASS_OR_PANE      = 1 << 19;
    static final int F_CAN_OCCLUDE        = 1 << 20;
    static final int F_FLUID_TRANSLUCENT  = 1 << 21;
    static final int F_TAG_LEAVES         = 1 << 22;

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: BlockProps <mc-version> <out.bin>");
            System.exit(2);
        }
        String mcVersion = args[0];
        String outPath = args[1];

        SharedConstants.tryDetectVersion();
        Bootstrap.bootStrap();

        // Blocks are registered lazily via the class initializer; touching it
        // before iterating the state registry is what fills BLOCK_STATE_REGISTRY.
        Blocks.AIR.defaultBlockState();

        // Intern block names and property tokens so the table stays compact.
        Map<String, Integer> blockIds = new LinkedHashMap<>();
        Map<String, Integer> propIds = new LinkedHashMap<>();
        // Only a few dozen distinct fluid legacy states exist (air + water and
        // lava levels), so states reference them through a one-byte index.
        Map<Integer, Integer> fluidLegacyIds = new LinkedHashMap<>();
        List<int[]> rows = new ArrayList<>();      // flags, emission, lightBlock, blockIdx, fluidLegacyIdx
        List<int[]> rowProps = new ArrayList<>();

        BlockPos origin = BlockPos.ZERO;
        int n = Block.BLOCK_STATE_REGISTRY.size();
        int renderTypeFailures = 0;

        for (int id = 0; id < n; id++) {
            BlockState state = Block.BLOCK_STATE_REGISTRY.byId(id);
            if (state == null) {
                throw new IllegalStateException("null state at id " + id);
            }
            Block b = state.getBlock();
            String blockName = BuiltInRegistries.BLOCK.getKey(b).toString();
            int blockIdx = blockIds.computeIfAbsent(blockName, k -> blockIds.size());

            int flags = 0;
            if (state.isAir()) flags |= F_AIR;
            if (!(b instanceof LiquidBlock) && state.getRenderShape() == RenderShape.INVISIBLE) {
                flags |= F_RENDER_INVISIBLE;
            }
            // Mirrors MapWriter.hasVanillaColor: a throwing or zero map colour
            // means the block is skipped as a surface candidate.
            MapColor mapColor = null;
            try {
                mapColor = state.getMapColor(EmptyBlockGetter.INSTANCE, origin);
            } catch (Throwable ignored) {
                // Same swallow the mod does; leaves F_HAS_MAP_COLOR clear.
            }
            if (mapColor != null && mapColor.col != 0) flags |= F_HAS_MAP_COLOR;

            FluidState fluid = state.getFluidState();
            BlockState fluidLegacy = fluid.createLegacyBlock();
            int fluidLegacyId = Block.BLOCK_STATE_REGISTRY.getId(fluidLegacy);
            if (!fluid.isEmpty()) {
                if (fluid.getType().isSame(net.minecraft.world.level.material.Fluids.WATER)) flags |= F_FLUID_WATER;
                if (fluid.getType().isSame(net.minecraft.world.level.material.Fluids.LAVA)) flags |= F_FLUID_LAVA;
                try {
                    if (ItemBlockRenderTypes.getRenderLayer(fluid) == RenderType.translucent()) {
                        flags |= F_FLUID_TRANSLUCENT;
                    }
                } catch (Throwable t) {
                    renderTypeFailures++;
                }
            }
            if (b instanceof LiquidBlock) flags |= F_LIQUID_BLOCK;
            if (state.canBeReplaced()) flags |= F_CAN_BE_REPLACED;
            if (state.ignitedByLava()) flags |= F_IGNITED_BY_LAVA;
            if (state.getPistonPushReaction() == PushReaction.DESTROY) flags |= F_PUSH_DESTROY;
            try {
                if (ItemBlockRenderTypes.getChunkRenderType(state) == RenderType.translucent()) {
                    flags |= F_TRANSLUCENT_LAYER;
                }
            } catch (Throwable t) {
                renderTypeFailures++;
            }
            try {
                if (Block.isShapeFullBlock(state.getShape(EmptyBlockGetter.INSTANCE, origin))) {
                    flags |= F_SHAPE_FULL_BLOCK;
                }
            } catch (Throwable ignored) {
                // Shapes that need real world context: treat as not full.
            }
            if (b instanceof TransparentBlock) flags |= F_TRANSPARENT_CLASS;
            if (b instanceof HalfTransparentBlock) flags |= F_HALF_TRANSP_CLASS;
            if (b instanceof DoublePlantBlock) flags |= F_DOUBLE_PLANT;
            boolean isFlower = b instanceof PitcherCropBlock || b instanceof TallFlowerBlock
                    || b instanceof FlowerBlock
                    || (state.is(BlockTags.FLOWERS) && !state.is(BlockTags.LEAVES));
            if (isFlower) flags |= F_FLOWERISH;
            if (state.is(BlockTags.LEAVES)) flags |= F_TAG_LEAVES;
            if (b == Blocks.GRASS_BLOCK) flags |= F_GRASS_BLOCK;
            if (b == Blocks.WATER) flags |= F_WATER_BLOCK;
            if (b == Blocks.TORCH) flags |= F_TORCH;
            if (b == Blocks.SHORT_GRASS) flags |= F_SHORT_GRASS;
            if (b == Blocks.GLASS || b == Blocks.GLASS_PANE) flags |= F_GLASS_OR_PANE;
            if (state.canOcclude()) flags |= F_CAN_OCCLUDE;

            int fluidLegacyIdx = fluidLegacyIds.computeIfAbsent(fluidLegacyId, k -> fluidLegacyIds.size());
            rows.add(new int[] { flags, state.getLightEmission(), state.getLightBlock(), blockIdx, fluidLegacyIdx });

            // Property tokens in the same order the state reports them, so the
            // NBT written from this table matches the game's own ordering.
            List<Integer> mine = new ArrayList<>();
            for (Map.Entry<Property<?>, Comparable<?>> e : state.getValues().entrySet()) {
                Property<?> p = e.getKey();
                String token = p.getName() + "=" + getName(p, e.getValue());
                mine.add(propIds.computeIfAbsent(token, k -> propIds.size()));
            }
            int[] arr = new int[mine.size()];
            for (int i = 0; i < arr.length; i++) arr[i] = mine.get(i);
            rowProps.add(arr);
        }

        try (DataOutputStream out = new DataOutputStream(
                new BufferedOutputStream(new FileOutputStream(outPath)))) {
            out.writeBytes("XBP1");
            out.writeShort(1);
            writeStr(out, mcVersion);
            out.writeInt(blockIds.size());
            for (String s : blockIds.keySet()) writeStr(out, s);
            out.writeInt(propIds.size());
            for (String s : propIds.keySet()) writeStr(out, s);
            out.writeInt(fluidLegacyIds.size());
            for (int id : fluidLegacyIds.keySet()) out.writeInt(id);
            out.writeInt(n);
            for (int i = 0; i < n; i++) {
                int[] r = rows.get(i);
                out.writeInt(r[0]);
                out.writeByte(r[1]);
                out.writeByte(r[2]);
                out.writeShort(r[3]);
                out.writeByte(r[4]);
                int[] ps = rowProps.get(i);
                out.writeByte(ps.length);
                for (int p : ps) out.writeShort(p);
            }
        }
        if (blockIds.size() > 0xFFFF || propIds.size() > 0xFFFF || fluidLegacyIds.size() > 0xFF) {
            throw new IllegalStateException("a table outgrew its on-disk width; widen the format");
        }
        System.out.printf("wrote %s: %d states, %d blocks, %d property tokens, %d fluid states (MC %s)%n",
                outPath, n, blockIds.size(), propIds.size(), fluidLegacyIds.size(), mcVersion);
        if (renderTypeFailures > 0) {
            System.out.printf("WARNING: %d render-type lookups failed; translucency flags are incomplete%n",
                    renderTypeFailures);
        }
    }

    @SuppressWarnings({ "unchecked", "rawtypes" })
    private static <T extends Comparable<T>> String getName(Property<T> property, Comparable<?> value) {
        return property.getName((T) value);
    }

    private static void writeStr(DataOutputStream out, String s) throws Exception {
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        out.writeShort(b.length);
        out.write(b);
    }
}
